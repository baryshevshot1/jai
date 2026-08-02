// ── Модели: установка, набор приложения, локальное состояние и обновления ────
// Всё про модели движка: скачивание с прогрессом (/api/pull), список установленных,
// зафиксированный набор приложения (MODEL_SET) и сверка обновлений по digest.

use serde::Serialize;
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::ipc::Channel;

use crate::error::{AppError, AppResult};
use crate::{tools, with_cancel, HTTP, OLLAMA_META_TIMEOUT};

/// Активная установка модели (одна на процесс): имя тега + флаг отмены ИМЕННО этой
/// задачи. Отдельно от CancelFlag чата (отмена скачивания не гасит идущий чат).
/// Параллельные установки запрещены: раньше общий флаг давал гонку — старт второго
/// pull сбрасывал отмену первого, и тот молча продолжал качать после «Отмены».
pub(crate) struct PullJob {
    pub(crate) name: String,
    pub(crate) cancel: Arc<AtomicBool>,
}
pub(crate) struct PullState(pub(crate) std::sync::Mutex<Option<PullJob>>);

/// Гард регистрации установки: снимает задачу при ЛЮБОМ выходе (успех, ошибка,
/// отмена, сброс future) — «залипшая» регистрация невозможна. Общий для скачивания
/// (pull_model) и импорта с диска (provision) — они взаимоисключаемы.
pub(crate) struct PullJobGuard<'a>(pub(crate) &'a PullState);
impl Drop for PullJobGuard<'_> {
    fn drop(&mut self) {
        let state = self.0;
        state.0.lock().unwrap_or_else(|e| e.into_inner()).take();
    }
}

/// Отсечка молчания сети при установке: столько подряд без единого байта — честная
/// ошибка (докачка продолжится с места при повторе), а не вечный «прогресс».
const PULL_STALL: std::time::Duration = std::time::Duration::from_secs(120);

// ── Установка моделей (операционный слой): /api/pull с прогрессом ─────────────

/// Прогресс установки модели. status — стадия (manifest/downloading/verify/…),
/// completed/total — байты текущего слоя (0/0 у статусных строк без чисел).
/// Итог (успех/отмена) идёт НЕ событием, а результатом команды — см. PullOutcome.
#[derive(Clone, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub(crate) enum PullEvent {
    Progress {
        status: String,
        completed: u64,
        total: u64,
    },
}

/// Итог установки: завершена или отменена пользователем. Раньше отмена возвращала
/// тот же Ok(()), что и успех, — интерфейс не мог их отличить и писал «установлено»
/// после отменённой закачки. На проводе — строки "done" | "cancelled".
#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub(crate) enum PullOutcome {
    Done,
    Cancelled,
}

/// Установка модели в Ollama (`POST /api/pull`, stream:true). Тянет модель из
/// интернета (онлайн-провижининг). Поток NDJSON разбираем тем же способом, что и
/// chat_stream; прогресс шлём через Channel; отмена — по флагу СВОЕЙ задачи.
/// Одна установка на процесс: вторая параллельная честно отклоняется.
/// Только localhost — наружу ходит сама Ollama, не приложение.
#[tauri::command]
pub(crate) async fn pull_model(
    name: String,
    on_event: Channel<PullEvent>,
    state: tauri::State<'_, PullState>,
) -> Result<PullOutcome, String> {
    // Регистрация задачи — в синхронной области (std-мьютекс не держим через await);
    // гард ниже снимет регистрацию при любом выходе из функции.
    let my_cancel = {
        let mut job = state.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(active) = job.as_ref() {
            return Err(format!(
                "Уже идёт установка «{}» — дождитесь её завершения или отмените.",
                active.name
            ));
        }
        let cancel = Arc::new(AtomicBool::new(false));
        *job = Some(PullJob { name: name.clone(), cancel: cancel.clone() });
        cancel
    };
    let _guard = PullJobGuard(&state);

    let body = serde_json::json!({ "name": name, "stream": true });

    // Подключение и заголовки ответа — с отменой и потолком ожидания: Ollama может
    // принять соединение и замолчать (занята, залипла в свопе) — future без таймаута
    // висел бы вечно, а занятая регистрация блокировала бы ЛЮБЫЕ установки до
    // перезапуска приложения. PULL_STALL начинает действовать лишь после ответа.
    let send_fut = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        HTTP.post("http://127.0.0.1:11434/api/pull").json(&body).send(),
    );
    let mut resp = match with_cancel(send_fut, &my_cancel).await {
        None => return Ok(PullOutcome::Cancelled), // «Отмена» ещё до ответа движка
        Some(Err(_elapsed)) => {
            return Err("Движок не ответил на запрос установки — попробуйте ещё раз \
                        (если не поможет, перезапустите приложение)."
                .into())
        }
        Some(Ok(r)) => r.map_err(|e| format!("Не удалось подключиться к Ollama: {e}"))?,
    };

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("Ollama вернул ошибку {status}: {text}"));
    }

    // Разбор одной NDJSON-строки: статус/прогресс или ошибка (напр. нет интернета).
    let handle = |line: &str, on_event: &Channel<PullEvent>| -> Result<(), String> {
        let json: serde_json::Value = match serde_json::from_str(line) {
            Ok(j) => j,
            Err(_) => return Ok(()), // неполная/служебная строка — пропускаем
        };
        // Ошибка приходит полем "error" (нет сети, неизвестная модель и т. п.).
        if let Some(err) = json.get("error").and_then(|e| e.as_str()) {
            return Err(format!(
                "Не удалось скачать модель: {err}. Нужен доступ в интернет \
                 (офлайн-установка моделей с диска появится отдельным механизмом)."
            ));
        }
        let status = json
            .get("status")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();
        let completed = json.get("completed").and_then(|v| v.as_u64()).unwrap_or(0);
        let total = json.get("total").and_then(|v| v.as_u64()).unwrap_or(0);
        let _ = on_event.send(PullEvent::Progress {
            status,
            completed,
            total,
        });
        Ok(())
    };

    // Тот же байтовый разбор NDJSON, что в chat_stream (UTF-8 на границе чанков,
    // дочитка хвоста). Чанки ждём короткими окнами: отмена срабатывает, ДАЖЕ когда
    // сеть молчит; затянувшееся молчание (PULL_STALL) — честная ошибка вместо
    // вечного «идёт установка».
    let mut buf: Vec<u8> = Vec::new();
    let mut last_data = std::time::Instant::now();
    loop {
        if my_cancel.load(Ordering::Relaxed) {
            // отмена: рвём соединение; Ollama останавливает закачку и докачает с места при повторе
            return Ok(PullOutcome::Cancelled);
        }
        let chunk = match tokio::time::timeout(
            std::time::Duration::from_millis(400),
            resp.chunk(),
        )
        .await
        {
            Err(_elapsed) => {
                if last_data.elapsed() > PULL_STALL {
                    return Err("Сеть не отвечает — установка прервана. Проверьте \
                                интернет и повторите: докачка продолжится с места."
                        .into());
                }
                continue; // окно без данных — перепроверяем отмену
            }
            Ok(Ok(Some(chunk))) => {
                last_data = std::time::Instant::now();
                chunk
            }
            Ok(Ok(None)) => break, // поток корректно завершился
            Ok(Err(e)) => return Err(format!("Сбой сети при скачивании модели: {e}")),
        };
        buf.extend_from_slice(&chunk);
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line_bytes: Vec<u8> = buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line_bytes);
            let line = line.trim();
            if !line.is_empty() {
                handle(line, &on_event)?;
            }
        }
    }
    if !buf.is_empty() {
        let line = String::from_utf8_lossy(&buf);
        let line = line.trim();
        if !line.is_empty() {
            handle(line, &on_event)?;
        }
    }

    Ok(PullOutcome::Done)
}

/// Отмена текущей установки модели: взводит флаг АКТИВНОЙ задачи (если она есть).
/// Итог придёт результатом pull_model (Cancelled) — интерфейс отличит его от успеха.
#[tauri::command]
pub(crate) fn cancel_pull(state: tauri::State<'_, PullState>) {
    if let Some(job) = state.0.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        job.cancel.store(true, Ordering::Relaxed);
    }
}

/// Модель + поддержка рассуждений ("thinking"), зрения ("vision") и вызова
/// инструментов ("tools") из capabilities. `tools` гейтит онлайн-режим: без неё
/// модель не умеет возвращать tool_calls, поэтому агентный веб-поиск недоступен.
#[derive(Serialize)]
pub(crate) struct ModelInfo {
    name: String,
    thinking: bool,
    vision: bool,
    tools: bool,
}

/// Список установленных моделей из Ollama: GET 127.0.0.1:11434/api/tags.
/// Возвращаем имя и поддержку рассуждений (по полю capabilities). Только localhost.
#[tauri::command]
pub(crate) async fn list_models() -> AppResult<Vec<ModelInfo>> {
    let resp = HTTP
        .get("http://127.0.0.1:11434/api/tags")
        .timeout(OLLAMA_META_TIMEOUT)
        .send()
        .await
        .map_err(|e| AppError::from_reqwest(&e, "Список моделей недоступен"))?;

    if !resp.status().is_success() {
        return Err(AppError::unknown(format!("Движок вернул ошибку {}", resp.status())));
    }

    let json: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    let models = json
        .get("models")
        .and_then(|m| m.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    let name = m.get("name").and_then(|n| n.as_str())?.to_string();
                    let caps = m.get("capabilities").and_then(|c| c.as_array());
                    let has = |cap: &str| {
                        caps.map(|c| c.iter().any(|x| x.as_str() == Some(cap)))
                            .unwrap_or(false)
                    };
                    // Модели эмбеддингов (bge-m3) не умеют отвечать — в селекторе чата
                    // им не место: выбор такой модели ронял бы любой вопрос. Если
                    // capabilities не отдаются (старый Ollama) — модель оставляем.
                    if has("embedding") {
                        return None;
                    }
                    Some(ModelInfo {
                        name,
                        thinking: has("thinking"),
                        vision: has("vision"),
                        tools: has("tools"),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(models)
}

// ── Управление моделями (M1): явный набор нужных моделей + локальное состояние ──

/// Описание модели из нужного набора. Список ниже — единственное место правки,
/// чтобы добавлять/менять модели (теги/роли) без переписывания логики.
pub(crate) struct ModelSpec {
    pub(crate) tag: &'static str,   // точный тег Ollama (по нему идут pull и сравнение digest)
    pub(crate) role: &'static str,  // "text" | "embed" | "vision" | "code"
    pub(crate) title: &'static str, // человекочитаемое название
    pub(crate) required: bool, // обязательная (без неё ломается сценарий) или опциональная
    /// Приблизительный вес на диске, ГБ — для оценки посильности и места ДО установки
    /// (после установки берётся точный размер из /api/tags). Источник: реестр Ollama.
    pub(crate) approx_gb: f64,
    /// Участвует ли модель в АВТОМАТИЧЕСКОМ подборе по задаче и железу. false —
    /// профиль под ручной выбор: приложение само его не назначит, даже если он
    /// установлен и подходит по роли (так задуман русский профиль T-lite).
    pub(crate) auto_pick: bool,
}

/// Нужный набор моделей приложения. Расширяется правкой ОДНОГО этого списка.
/// Теги — реальные (фактически устанавливаемые), не абстрактные.
pub(crate) const MODEL_SET: &[ModelSpec] = &[
    ModelSpec { tag: "qwen3.5:9b", role: "text", title: "Базовая текстовая (чат)", required: true, approx_gb: 6.6, auto_pick: true },
    ModelSpec { tag: "qwen3.5:4b", role: "text", title: "Текстовая (лёгкая, для слабого железа)", required: false, approx_gb: 3.0, auto_pick: true },
    // Русский профиль — на Hugging Face (в реестре Ollama под t-tech/ его нет).
    // auto_pick: false — его назначают вручную в настройках, сам он не подставляется.
    ModelSpec { tag: "hf.co/t-tech/T-lite-it-2.1-GGUF:Q4_K_M", role: "text", title: "Русский профиль (T-lite)", required: false, approx_gb: 5.1, auto_pick: false },
    ModelSpec { tag: "bge-m3:latest", role: "embed", title: "Поиск по документам (RAG)", required: true, approx_gb: 1.2, auto_pick: false },
    ModelSpec { tag: "qwen3-vl:8b", role: "vision", title: "Зрение и OCR", required: false, approx_gb: 6.5, auto_pick: true },
    ModelSpec { tag: "qwen3-vl:4b", role: "vision", title: "Зрение (лёгкая)", required: false, approx_gb: 3.5, auto_pick: true },
    ModelSpec { tag: "qwen3-coder:latest", role: "code", title: "Код", required: false, approx_gb: 19.0, auto_pick: false },
];

/// Локальное состояние одной модели набора (без сети). digest/size/date — если установлена.
#[derive(Serialize)]
pub(crate) struct ModelState {
    pub(crate) tag: String,
    role: String,
    title: String,
    /// Вес и признак авто-подбора уходят на фронт не для показа, а чтобы он сам
    /// строил лестницу «старшая → лёгкая» внутри роли. Иначе этот порядок пришлось
    /// бы держать вторым списком в TypeScript, и он молча разошёлся бы с набором.
    approx_gb: f64,
    auto_pick: bool,
    pub(crate) required: bool,
    pub(crate) installed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    digest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    modified_at: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct ModelStatesResult {
    pub(crate) models: Vec<ModelState>, // модели набора (в порядке списка)
    others: Vec<String>,                // установленные вне набора — «прочие», не мешаем
}

/// Локальные состояния моделей набора по `/api/tags` (без сети): установлена ли,
/// локальный digest/размер/дата. Статус обновления считается отдельно (M2, онлайн).
#[tauri::command]
pub(crate) async fn model_states() -> AppResult<ModelStatesResult> {
    let resp = HTTP
        .get("http://127.0.0.1:11434/api/tags")
        .timeout(OLLAMA_META_TIMEOUT)
        .send()
        .await
        .map_err(|e| AppError::from_reqwest(&e, "Список моделей недоступен"))?;
    if !resp.status().is_success() {
        return Err(AppError::unknown(format!("Движок вернул ошибку {}", resp.status())));
    }
    let json: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    let empty: Vec<serde_json::Value> = Vec::new();
    let arr = json.get("models").and_then(|m| m.as_array()).unwrap_or(&empty);

    let mut models = Vec::new();
    for spec in MODEL_SET {
        let m = arr
            .iter()
            .find(|m| m.get("name").and_then(|n| n.as_str()) == Some(spec.tag));
        models.push(ModelState {
            tag: spec.tag.to_string(),
            role: spec.role.to_string(),
            title: spec.title.to_string(),
            approx_gb: spec.approx_gb,
            auto_pick: spec.auto_pick,
            required: spec.required,
            installed: m.is_some(),
            digest: m.and_then(|m| m.get("digest")).and_then(|d| d.as_str()).map(str::to_string),
            size: m.and_then(|m| m.get("size")).and_then(|s| s.as_u64()),
            modified_at: m
                .and_then(|m| m.get("modified_at"))
                .and_then(|s| s.as_str())
                .map(str::to_string),
        });
    }
    let others: Vec<String> = arr
        .iter()
        .filter_map(|m| m.get("name").and_then(|n| n.as_str()))
        .filter(|name| !MODEL_SET.iter().any(|s| s.tag == *name))
        .map(str::to_string)
        .collect();
    Ok(ModelStatesResult { models, others })
}

// ── Проверка обновлений по digest (M2): онлайн, по явному запросу ─────────────

/// Статус обновления одной модели набора.
/// status: "current" (актуальна) | "update" (есть обновление) | "not_installed" |
/// "unsupported" (проверка для этой модели недоступна) | "error".
#[derive(Serialize)]
pub(crate) struct UpdateStatus {
    tag: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

/// Локальные digest по тегам из `/api/tags` (без сети).
async fn local_digests() -> AppResult<std::collections::HashMap<String, String>> {
    let resp = HTTP
        .get("http://127.0.0.1:11434/api/tags")
        .timeout(OLLAMA_META_TIMEOUT)
        .send()
        .await
        .map_err(|e| AppError::from_reqwest(&e, "Список моделей недоступен"))?;
    let json: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    let mut map = std::collections::HashMap::new();
    if let Some(arr) = json.get("models").and_then(|m| m.as_array()) {
        for m in arr {
            if let (Some(name), Some(dig)) = (
                m.get("name").and_then(|n| n.as_str()),
                m.get("digest").and_then(|d| d.as_str()),
            ) {
                map.insert(name.to_string(), dig.to_string());
            }
        }
    }
    Ok(map)
}

/// digest манифеста тега в реестре Ollama = sha256 тела ответа (проверено: совпадает
/// с локальным digest). Требует HTTPS (rustls-tls) — единственный выход в интернет.
/// `client` — ИЗОЛИРОВАННЫЙ веб-клиент (tools::web_client: https_only, UA, таймауты
/// 8 с/20 с), НЕ localhost-клиент Ollama — правило разделения сетевых путей.
async fn registry_digest(client: &reqwest::Client, tag: &str) -> Result<String, String> {
    let (model, t) = tag.split_once(':').unwrap_or((tag, "latest"));
    let url = format!("https://registry.ollama.ai/v2/library/{model}/manifests/{t}");
    let resp = client
        .get(&url)
        .header("Accept", "application/vnd.docker.distribution.manifest.v2+json")
        .send()
        .await
        .map_err(|e| format!("Нет доступа к реестру (нужен интернет): {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("Реестр вернул {} для «{tag}»", resp.status()));
    }
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    let mut h = Sha256::new();
    h.update(&bytes);
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Проверить обновления моделей набора (онлайн, по запросу). Для установленных
/// сравнивает локальный digest с реестром. Без сети — каждая модель отдаёт "error".
#[tauri::command]
pub(crate) async fn check_model_updates() -> AppResult<Vec<UpdateStatus>> {
    // Наружу — только изолированным веб-клиентом (rustls, https_only, таймауты);
    // localhost-метаданные идут общим клиентом HTTP. Пути не смешиваются.
    let web = tools::web_client()?;
    let local = local_digests().await?;
    let mut out = Vec::new();
    for spec in MODEL_SET {
        let Some(local_dig) = local.get(spec.tag) else {
            out.push(UpdateStatus { tag: spec.tag.to_string(), status: "not_installed".into(), message: None });
            continue;
        };
        // Модели с Hugging Face (теги hf.co/…) в реестре Ollama не публикуются —
        // digest сверить не с чем, запрос вернул бы гарантированный 404. Отдельный
        // статус вместо ошибки: иначе интерфейс показывал бы «Нет сети» при
        // полностью рабочем интернете.
        if spec.tag.starts_with("hf.co/") {
            out.push(UpdateStatus {
                tag: spec.tag.to_string(),
                status: "unsupported".into(),
                message: Some("Для моделей Hugging Face проверка обновлений недоступна".into()),
            });
            continue;
        }
        match registry_digest(&web, spec.tag).await {
            Ok(remote) => {
                let status = if &remote == local_dig { "current" } else { "update" };
                out.push(UpdateStatus { tag: spec.tag.to_string(), status: status.into(), message: None });
            }
            Err(e) => {
                out.push(UpdateStatus { tag: spec.tag.to_string(), status: "error".into(), message: Some(e) });
            }
        }
    }
    Ok(out)
}

/// Размеры установленных моделей (имя → байты) из /api/tags — для подбора «полегче»
/// и оценки посильности набора (provision). Движок молчит → пустая карта.
pub(crate) async fn installed_sizes() -> std::collections::HashMap<String, u64> {
    let mut out = std::collections::HashMap::new();
    if let Ok(resp) = HTTP
        .get("http://127.0.0.1:11434/api/tags")
        .timeout(OLLAMA_META_TIMEOUT)
        .send()
        .await
    {
        if let Ok(tags) = resp.json::<serde_json::Value>().await {
            if let Some(arr) = tags.get("models").and_then(|m| m.as_array()) {
                for m in arr {
                    if let (Some(n), Some(sz)) = (
                        m.get("name").and_then(|n| n.as_str()),
                        m.get("size").and_then(|s| s.as_u64()),
                    ) {
                        out.insert(n.to_string(), sz);
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// MODEL_SET объявлен единственным местом правки набора, и на него опирается
    /// фронтенд (роль, вес и auto_pick уезжают в model_states). Тест ловит опечатки
    /// в этом списке до того, как они превратятся в «приложение предлагает не ту
    /// модель» на машине клиента.
    #[test]
    fn model_set_is_consistent() {
        let mut seen = std::collections::HashSet::new();
        for spec in MODEL_SET {
            assert!(seen.insert(spec.tag), "тег «{}» встречается дважды", spec.tag);
            assert!(!spec.tag.is_empty(), "пустой тег в наборе");
            assert!(!spec.title.is_empty(), "у «{}» нет имени для человека", spec.tag);
            assert!(spec.approx_gb > 0.0, "у «{}» нулевой вес", spec.tag);
            assert!(
                matches!(spec.role, "text" | "embed" | "vision" | "code"),
                "неизвестная роль «{}» у «{}»",
                spec.role,
                spec.tag
            );
        }

        // Роли, которые подбираются автоматически, обязаны иметь хотя бы одну модель:
        // пустая лестница означала бы, что подбирать не из чего и чат молча не работает.
        for role in ["text", "vision"] {
            assert!(
                MODEL_SET.iter().any(|s| s.role == role && s.auto_pick),
                "в роли «{role}» не осталось ни одной модели для авто-подбора"
            );
        }

        // Внутри роли веса должны различаться — иначе «старшая» и «лёгкая»
        // неотличимы, и порядок лестницы станет случайным.
        for role in ["text", "vision"] {
            let mut weights: Vec<String> = MODEL_SET
                .iter()
                .filter(|s| s.role == role && s.auto_pick)
                .map(|s| format!("{:.2}", s.approx_gb))
                .collect();
            let total = weights.len();
            weights.sort();
            weights.dedup();
            assert_eq!(weights.len(), total, "в роли «{role}» есть модели с одинаковым весом");
        }

        // Модель поиска по документам ровно одна: фронт берёт её как tagForRole("embed").
        let embed = MODEL_SET.iter().filter(|s| s.role == "embed").count();
        assert_eq!(embed, 1, "модель эмбеддингов должна быть ровно одна, найдено {embed}");
    }
}
