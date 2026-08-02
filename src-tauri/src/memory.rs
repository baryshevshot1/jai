// ── Память и план запуска: оценка «поместится ли» + лестница смягчения ──────
// S1: сколько памяти нужно модели при заданном контексте. S2: как смягчить запрос —
// снизить контекст, взять модель полегче или честно отказать.

use serde::Serialize;

use crate::diagnostics::detect_vram;
use crate::engine;
use crate::models::{installed_sizes, ModelSpec, MODEL_SET};
use crate::error::{AppError, AppResult};
use crate::{HTTP, OLLAMA_META_TIMEOUT};

// ── Стабильность (S1): оценка памяти по формуле (вес + KV + буфер + запас) ─────

pub(crate) const GB: f64 = 1024.0 * 1024.0 * 1024.0;
/// Запас под ОС и прочие процессы — отделяет «помещается» от «впритык». Консервативно.
pub(crate) const MEM_SAFETY_GB: f64 = 1.5;
/// Байт на элемент KV-кэша при q8_0 — экономный режим НАШЕГО движка (engine.rs).
pub(crate) const KV_BYTES_Q8: f64 = 1.1;
/// Байт на элемент KV-кэша при f16 — так по умолчанию работает ВНЕШНИЙ движок:
/// env-переменные экономного режима действуют только на процесс, поднятый нами.
pub(crate) const KV_BYTES_F16: f64 = 2.0;
/// Compute-буфер (с flash attention почти не растёт с контекстом).
pub(crate) const COMPUTE_BUF_GB: f64 = 0.6;

/// Доступная сейчас оперативная память, ГБ (кроссплатформенно, sysinfo).
fn available_ram_gb() -> f64 {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    sys.available_memory() as f64 / GB
}

/// Всего оперативной памяти, ГБ. Нужна только на macOS — на Linux и Windows
/// достаточно точного `available`, поэтому там функции просто нет.
#[cfg(target_os = "macos")]
fn total_ram_gb() -> f64 {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    sys.total_memory() as f64 / GB
}

/// Реалистично доступная под модель память, ГБ. На macOS «available» резко
/// недосчитывает (агрессивный файловый кэш реклеймится под новые аллокации), поэтому
/// берём пол ~55% от total; на Linux/Windows доверяем точному available.
pub(crate) fn usable_ram_gb() -> f64 {
    #[cfg(target_os = "macos")]
    {
        available_ram_gb().max(total_ram_gb() * 0.55)
    }
    #[cfg(not(target_os = "macos"))]
    {
        available_ram_gb()
    }
}

/// Общий вердикт «поместится ли»: сколько ляжет именно на RAM после вычета VRAM
/// (своп — главный риск; на дискретном GPU часть до объёма VRAM не давит на RAM)
/// против доступной памяти. Используется и в estimate() (установленные модели,
/// точный KV из метаданных), и в оценке набора ДО установки (provision, эвристика).
pub(crate) fn fit_verdict(
    required_gb: f64,
    vram_gb: Option<f64>,
    available_gb: f64,
) -> (&'static str, &'static str) {
    let (ram_needed, limiting) = match vram_gb {
        Some(vram) => ((required_gb - vram).max(0.0), "vram"),
        None => (required_gb, "ram"),
    };
    let fits = if ram_needed <= available_gb {
        "ok"
    } else if ram_needed - MEM_SAFETY_GB <= available_gb {
        "tight"
    } else {
        "no"
    };
    (fits, limiting)
}

/// Значение из model_info по суффиксу ключа (ключи префиксованы архитектурой,
/// напр. `qwen35.block_count` — ищем по `.block_count`).
fn mi_u64(mi: &serde_json::Map<String, serde_json::Value>, suffix: &str) -> Option<u64> {
    mi.iter()
        .find(|(k, _)| k.ends_with(suffix))
        .and_then(|(_, v)| v.as_u64())
}

/// Оценка памяти: ok / tight / no + лимитирующий ресурс и числа (ГБ).
#[derive(Serialize, Clone)]
struct MemoryEstimate {
    fits: String,     // "ok" | "tight" | "no"
    limiting: String, // "ram" | "vram"
    required_gb: f64,
    available_gb: f64,
    weight_gb: f64,
    kv_gb: f64,
    vram_tight: bool, // вес+KV не помещаются в СВОБОДНУЮ видеопамять (будет медленнее)
}

/// Оценить, поместится ли модель при заданном контексте: вес (size из /api/tags) +
/// KV-кэш (метаданные /api/show × num_ctx × байт на элемент) + compute-буфер + запас,
/// против доступной памяти. На дискретном GPU часть сверх VRAM уходит в RAM (своп —
/// главный риск); на унифицированной памяти (Apple, VRAM=None) всё в общей RAM.
/// `kv_bytes_per_elem` — честный тип KV-кэша (q8_0 у нашего движка, f16 у внешнего).
/// `vram_gb`/`vram_free_gb` детектируются ОДИН РАЗ вызывающим: plan_inference зовёт
/// estimate до ~9 раз, а detect_vram — это запуск внешней утилиты (nvidia-smi).
async fn estimate(
    model: &str,
    num_ctx: u64,
    kv_bytes_per_elem: f64,
    vram_gb: Option<f64>,
    vram_free_gb: Option<f64>,
) -> AppResult<MemoryEstimate> {
    // Что сейчас в памяти (/api/ps) — первым. Прочие модели при MAX_LOADED_MODELS=1
    // выгрузятся под новую → их размер (и занятая ими видеопамять) прибавляется к
    // доступному. У целевой запоминаем контекст загруженного экземпляра.
    let mut loaded_total: u64 = 0;
    let mut loaded_vram: u64 = 0;
    let mut own_size: u64 = 0; // целевая модель, если уже загружена
    let mut own_vram: u64 = 0;
    let mut own_ctx: Option<u64> = None; // context_length загруженного экземпляра
    let mut already_loaded = false;
    if let Ok(resp) = HTTP
        .get("http://127.0.0.1:11434/api/ps")
        .timeout(OLLAMA_META_TIMEOUT)
        .send()
        .await
    {
        if let Ok(ps) = resp.json::<serde_json::Value>().await {
            if let Some(arr) = ps.get("models").and_then(|m| m.as_array()) {
                for m in arr {
                    let name = m.get("name").and_then(|n| n.as_str());
                    let size = m.get("size").and_then(|s| s.as_u64()).unwrap_or(0);
                    let size_vram = m.get("size_vram").and_then(|s| s.as_u64()).unwrap_or(0);
                    if name == Some(model) {
                        already_loaded = true;
                        own_size = size;
                        own_vram = size_vram;
                        own_ctx = m.get("context_length").and_then(|c| c.as_u64());
                    } else {
                        loaded_total += size;
                        loaded_vram += size_vram;
                    }
                }
            }
        }
    }
    // Хот-путь «уже загружена» (без тяжёлого /api/show) — только когда экземпляр
    // держит KV-кэш НЕ МЕНЬШЕ запрошенного контекста: тогда запрос не требует ни
    // байта сверх занятого. Меньший контекст (после downscale прошлого запроса)
    // означает ПЕРЕЗАГРУЗКУ модели с KV в разы больше — безусловное «ok» здесь
    // загоняло бы слабую машину в своп ровно там, где лестница S2 уже отступила.
    // Старый Ollama без context_length в /api/ps — тоже полная оценка (не знаем,
    // с каким контекстом загружено).
    if already_loaded && own_ctx.is_some_and(|c| c >= num_ctx) {
        return Ok(MemoryEstimate {
            fits: "ok".into(),
            limiting: "ram".into(),
            required_gb: 0.0,
            available_gb: usable_ram_gb() + loaded_total as f64 / GB,
            weight_gb: 0.0,
            kv_gb: 0.0,
            vram_tight: false,
        });
    }
    // Перезагрузка под больший контекст сначала выгрузит текущий экземпляр —
    // его память честно возвращается в доступное наравне с прочими моделями.
    if already_loaded {
        loaded_total += own_size;
        loaded_vram += own_vram;
    }
    let available_gb = usable_ram_gb() + loaded_total as f64 / GB;

    // вес = размер GGUF из /api/tags
    let tags: serde_json::Value = HTTP
        .get("http://127.0.0.1:11434/api/tags")
        .timeout(OLLAMA_META_TIMEOUT)
        .send()
        .await
        .map_err(|e| AppError::from_reqwest(&e, "Движок недоступен"))?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    let weight_bytes = tags
        .get("models")
        .and_then(|m| m.as_array())
        .and_then(|arr| {
            arr.iter()
                .find(|m| m.get("name").and_then(|n| n.as_str()) == Some(model))
        })
        .and_then(|m| m.get("size"))
        .and_then(|s| s.as_u64())
        .unwrap_or(0);
    let weight_gb = weight_bytes as f64 / GB;

    // KV-кэш из метаданных /api/show
    let show: serde_json::Value = HTTP
        .post("http://127.0.0.1:11434/api/show")
        .timeout(OLLAMA_META_TIMEOUT)
        .json(&serde_json::json!({ "model": model }))
        .send()
        .await
        .map_err(|e| AppError::from_reqwest(&e, "Движок недоступен"))?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    let empty = serde_json::Map::new();
    let mi = show.get("model_info").and_then(|m| m.as_object()).unwrap_or(&empty);

    let kv_gb = match (mi_u64(mi, ".block_count"), mi_u64(mi, ".attention.head_count")) {
        (Some(blocks), Some(heads)) => {
            // GQA: если head_count_kv нет — допущение 1/4 голов (типичный GQA), а не
            // head_count (MHA-worst-case завышал бы KV в разы для современных моделей).
            let kv_heads = mi_u64(mi, ".attention.head_count_kv").unwrap_or((heads / 4).max(1));
            let key_len = mi_u64(mi, ".attention.key_length").unwrap_or(128);
            let val_len = mi_u64(mi, ".attention.value_length").unwrap_or(key_len);
            // KV = слои × KV-головы × (K_dim + V_dim) × байты × токены
            let elems = blocks * kv_heads * (key_len + val_len) * num_ctx;
            elems as f64 * kv_bytes_per_elem / GB
        }
        // метаданных нет → консервативная оценка KV как доля веса под контекст
        // (доля калибрована под q8_0 — для f16 пропорционально больше)
        _ => weight_gb * 0.2 * (num_ctx as f64 / 8192.0) * (kv_bytes_per_elem / KV_BYTES_Q8),
    };

    let required_gb = weight_gb + kv_gb + COMPUTE_BUF_GB + MEM_SAFETY_GB;
    let (fits, limiting) = fit_verdict(required_gb, vram_gb, available_gb);

    // Отдельное честное наблюдение: помещаются ли вес+KV в СВОБОДНУЮ видеопамять.
    // Свободная = замер сейчас + то, что освободит выгрузка прочих моделей (size_vram
    // из /api/ps). На вердикт fits не влияет (тот про RAM/своп) — идёт примечанием
    // «будет медленнее: часть слоёв уйдёт в ОЗУ».
    let vram_tight = match vram_free_gb {
        Some(free) => weight_gb + kv_gb > free + loaded_vram as f64 / GB,
        None => false,
    };

    Ok(MemoryEstimate {
        fits: fits.into(),
        limiting: limiting.into(),
        required_gb,
        available_gb,
        weight_gb,
        kv_gb,
        vram_tight,
    })
}

// ── Стабильность (S2): лестница смягчения и авто-подбор модели «вниз» ──────────

/// Ступени контекста: полный → ниже (меньше KV-кэш → меньше памяти).
const CTX_LADDER: [u64; 3] = [8192, 4096, 2048];

/// Человеческое название лимитирующего ресурса для сообщений.
fn ru_resource(limiting: &str) -> &'static str {
    match limiting {
        "vram" => "видеопамяти",
        _ => "оперативной памяти",
    }
}

/// Решение по запросу: что и с каким контекстом запускать.
#[derive(Serialize)]
pub(crate) struct InferencePlan {
    action: String,         // "ok" | "downscale" | "refuse"
    model: String,          // модель для запроса
    num_ctx: u64,           // контекст для запроса
    reason: Option<String>, // пояснение для downscale/refuse
    original_model: String, // что просил пользователь
    note: Option<String>,   // примечание (действие не меняет): напр., тесно в видеопамяти
}

/// Примечание к плану: вес+KV не помещаются в СВОБОДНУЮ видеопамять → часть слоёв
/// уйдёт в ОЗУ, ответ будет медленнее. Вердикт «помещается» это не меняет (тот про RAM).
fn vram_note(est: &MemoryEstimate) -> Option<String> {
    est.vram_tight.then(|| {
        "Модель не помещается целиком в свободную видеопамять — ответ будет медленнее \
         (часть слоёв уйдёт в оперативную память)."
            .to_string()
    })
}

/// Лестница смягчения ПЕРЕД запуском (S2): экономрежим (K) уже применён → пробуем
/// снизить контекст у запрошенной модели → если не помещается, подбираем более лёгкую
/// модель той же роли «вниз» → если ничего, честный отказ с дефицитом. Только «вниз»;
/// ручной выбор не меняется (downscale действует лишь на этот запрос).
#[tauri::command]
pub(crate) async fn plan_inference(
    model: String,
    engine: tauri::State<'_, engine::EngineState>,
) -> AppResult<InferencePlan> {
    // Честный KV: q8_0 действует, только когда движок подняли мы (env-переменные
    // экономного режима); внешний движок по умолчанию держит KV в f16 (~вдвое больше).
    let kv_bytes = if engine::was_started_by_us(&engine) { KV_BYTES_Q8 } else { KV_BYTES_F16 };
    // Видеопамять детектируем один раз на весь план: estimate вызывается до ~9 раз,
    // а detect_vram запускает внешнюю утилиту (блокирующий вызов — уводим из
    // async-воркера в пул потоков). free — честная свободная, не total.
    let (vram_gb, vram_free_gb, _) = tauri::async_runtime::spawn_blocking(detect_vram)
        .await
        .map_err(|e| format!("Сбой определения видеопамяти: {e}"))?;

    // 1) запрошенная модель: пробуем снижать контекст
    for &ctx in &CTX_LADDER {
        let est = estimate(&model, ctx, kv_bytes, vram_gb, vram_free_gb).await?;
        if est.fits != "no" {
            let action = if ctx == CTX_LADDER[0] { "ok" } else { "downscale" };
            let reason = (ctx != CTX_LADDER[0]).then(|| {
                format!(
                    "контекст снижен до {ctx} — для полного не хватало {}",
                    ru_resource(&est.limiting)
                )
            });
            return Ok(InferencePlan {
                action: action.into(),
                model: model.clone(),
                num_ctx: ctx,
                reason,
                original_model: model,
                note: vram_note(&est),
            });
        }
    }

    // 2) подбор более лёгкой модели той же роли «вниз» (только установленные)
    if let Some(role) = MODEL_SET.iter().find(|s| s.tag == model).map(|s| s.role) {
        let sizes = installed_sizes().await;
        let cur = sizes.get(model.as_str()).copied().unwrap_or(u64::MAX);
        let mut cands: Vec<(&ModelSpec, u64)> = MODEL_SET
            .iter()
            .filter(|s| s.role == role && s.tag != model)
            .filter_map(|s| sizes.get(s.tag).map(|&sz| (s, sz)))
            .filter(|(_, sz)| *sz < cur)
            .collect();
        cands.sort_by_key(|(_, sz)| *sz); // от самой лёгкой
        for (cand, _) in cands {
            for &ctx in &CTX_LADDER {
                let est = estimate(cand.tag, ctx, kv_bytes, vram_gb, vram_free_gb).await?;
                if est.fits != "no" {
                    return Ok(InferencePlan {
                        action: "downscale".into(),
                        model: cand.tag.to_string(),
                        num_ctx: ctx,
                        reason: Some(format!(
                            "для «{model}» не хватало {}",
                            ru_resource(&est.limiting)
                        )),
                        original_model: model,
                        note: vram_note(&est),
                    });
                }
            }
        }
    }

    // 3) ничего не помещается → честный отказ с дефицитом
    let last = *CTX_LADDER.last().unwrap();
    let est = estimate(&model, last, kv_bytes, vram_gb, vram_free_gb).await?;
    Ok(InferencePlan {
        action: "refuse".into(),
        model: model.clone(),
        num_ctx: last,
        reason: Some(format!(
            "не хватает {} (нужно ~{:.1} ГБ, доступно ~{:.1} ГБ)",
            ru_resource(&est.limiting),
            est.required_gb,
            est.available_gb
        )),
        original_model: model,
        note: None,
    })
}

#[cfg(test)]
mod tests {
    use super::fit_verdict;

    // Вердикт «поместится ли»: без GPU — против RAM; с GPU — на RAM давит только
    // остаток сверх VRAM; зона «впритык» — в пределах MEM_SAFETY_GB.
    #[test]
    fn fit_verdict_ram_and_vram() {
        assert_eq!(fit_verdict(6.0, None, 10.0), ("ok", "ram"));
        assert_eq!(fit_verdict(11.0, None, 10.0), ("tight", "ram")); // 11−1.5 ≤ 10 < 11
        assert_eq!(fit_verdict(20.0, None, 10.0), ("no", "ram"));
        // GPU 6 ГБ: требуется 8 → на RAM ляжет 2 — свободно
        assert_eq!(fit_verdict(8.0, Some(6.0), 10.0), ("ok", "vram"));
        // требуется 20 → на RAM ляжет 14 при доступных 10 — не помещается
        assert_eq!(fit_verdict(20.0, Some(6.0), 10.0), ("no", "vram"));
    }
}
