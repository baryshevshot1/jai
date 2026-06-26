// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/
mod chunk; // Чанкинг (Фаза B): резка текста на фрагменты по границам
mod docstore; // База документов (Фаза B): векторное хранилище SQLite + sqlite-vec
mod embed; // Эмбеддинги (Фаза B): bge-m3 через Ollama, батчем
mod engine; // Операционный слой: управление процессом движка Ollama
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::ipc::Channel;
use tauri::Manager;

/// Флаг отмены текущего стрима (нажата «Стоп» ИЛИ сработал watchdog). Arc — чтобы
/// watchdog-задача могла взвести его параллельно с чтением потока (тот же механизм).
struct CancelFlag(Arc<AtomicBool>);

/// Отдельный флаг отмены установки модели (`/api/pull`). Отдельный от CancelFlag,
/// чтобы отмена скачивания не гасила идущий чат и наоборот — они могут идти параллельно.
struct PullCancelFlag(AtomicBool);

/// Watchdog (S3): период замеров и порог роста свопа, при котором прерываем запрос.
/// Консервативно (≥512 МБ роста = модель реально сливается в своп), чтобы не рубить
/// валидные запросы из-за мелких колебаний. Лёгкий: замер раз в 1.5 с.
const WATCHDOG_INTERVAL_MS: u64 = 1500;
const SWAP_TRIP_BYTES: u64 = 512 * 1024 * 1024;

/// Сериализует доступ к settings.json. set_setting делает read-modify-write всего
/// файла; без этого лока две близкие записи (тема/модель/Thinking) могли бы прочитать
/// старое состояние и затереть ключи друг друга. Лок держим на время чтения+записи.
struct SettingsLock(std::sync::Mutex<()>);

/// Документ, прикреплённый к сообщению пользователя (для сохранения в истории).
/// Текст уже усечён фронтендом под бюджет контекста. В Ollama НЕ уходит —
/// при отправке фронт собирает чистые {role, content}, поэтому здесь doc = None.
#[derive(Clone, Serialize, Deserialize)]
struct DocAttachment {
    name: String,
    ext: String,
    text: String,
    chars: usize,
    #[serde(default)]
    truncated: bool,
}

/// Источник ответа: документ базы и номер фрагмента, на который опирался ответ
/// (Фаза B5). Сохраняется в истории, в Ollama не уходит.
#[derive(Clone, Serialize, Deserialize)]
struct SourceRef {
    filename: String,
    chunk_index: i64,
}

/// Одно сообщение в диалоге (роль + текст, опц. прикреплённый документ, опц.
/// источники из базы). Приходит с фронтенда. `doc`/`sources` сериализуются только
/// когда есть (skip None), иначе пустые поля улетали бы в запрос к Ollama.
#[derive(Clone, Serialize, Deserialize)]
struct ChatMessage {
    role: String,
    content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    doc: Option<DocAttachment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sources: Option<Vec<SourceRef>>,
    // Зрение (qwen3-vl): сырой base64 изображений. Ollama ждёт поле `images`
    // прямо в сообщении. Пустой массив не сериализуется (в текстовых не появляется).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    images: Vec<String>,
}

/// Кусочки, которые Rust шлёт обратно в окно по мере генерации ответа.
/// В TS придут как { type: "chunk", content: "..." } и { type: "done" }.
#[derive(Clone, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum ChatEvent {
    Chunk { content: String },
    Thinking { content: String },
    Done,
}

/// Стриминг ответа от Ollama. Фронт передаёт модель, историю и «канал»
/// `on_event`, в который мы по мере поступления шлём кусочки текста.
/// Весь сетевой трафик — строго на 127.0.0.1:11434 (правило проекта).
#[tauri::command]
async fn chat_stream(
    model: String,
    messages: Vec<ChatMessage>,
    think: bool,
    num_ctx: Option<u64>,
    on_event: Channel<ChatEvent>,
    cancel: tauri::State<'_, CancelFlag>,
) -> Result<String, String> {
    cancel.0.store(false, Ordering::Relaxed); // новый запрос — сбрасываем «Стоп»
    // Контекст — рычаг смягчения (S2): меньше num_ctx → меньше KV-кэш → меньше памяти.
    let ctx = num_ctx.unwrap_or(8192);
    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "model": model,
        "messages": messages,
        "stream": true,
        // Режим рассуждений (тумблер в интерфейсе). На обычных моделях игнорируется.
        "think": think,
        "options": {
            // Контекст по умолчанию у Ollama всего ~4096 — длинная история плюс
            // большое рассуждение его переполняют, и ответ обрывается. 8192 —
            // потолок из CLAUDE.md; при нехватке памяти снижается (S2).
            "num_ctx": ctx
        },
    });

    let mut resp = client
        .post("http://127.0.0.1:11434/api/chat")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Не удалось подключиться к Ollama: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("Ollama вернул ошибку {status}: {text}"));
    }

    // Watchdog (S3): лёгкая задача следит за РОСТОМ свопа во время генерации (своп —
    // триггер зависания). При опасном пороге взводит тот же CancelFlag (как «Стоп»),
    // чтобы прервать запрос ДО неотзывчивости, и записывает причину.
    let wd_flag = cancel.0.clone();
    let wd_reason: Arc<std::sync::Mutex<Option<String>>> = Arc::new(std::sync::Mutex::new(None));
    let wd_reason_task = wd_reason.clone();
    let watchdog = tauri::async_runtime::spawn(async move {
        let mut sys = sysinfo::System::new();
        sys.refresh_memory();
        let base_swap = sys.used_swap();
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(WATCHDOG_INTERVAL_MS)).await;
            if wd_flag.load(Ordering::Relaxed) {
                break; // уже отменено («Стоп» или генерация завершена)
            }
            sys.refresh_memory();
            let growth = sys.used_swap().saturating_sub(base_swap);
            if growth > SWAP_TRIP_BYTES {
                *wd_reason_task.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some("недостаточно оперативной памяти".to_string());
                wd_flag.store(true, Ordering::Relaxed); // прерываем тем же механизмом
                break;
            }
        }
    });

    // Ollama отдаёт поток NDJSON — по одному JSON-объекту на строку.
    // full — полный текст ответа (авторитетный результат, без гонок на фронте).
    let mut full = String::new();

    // Разбор одной NDJSON-строки: thinking / content / done.
    let handle = |line: &str, full: &mut String| {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(line) {
            // Рассуждения «думающих» моделей приходят отдельным полем thinking.
            if let Some(t) = json
                .get("message")
                .and_then(|m| m.get("thinking"))
                .and_then(|t| t.as_str())
            {
                if !t.is_empty() {
                    let _ = on_event.send(ChatEvent::Thinking { content: t.to_string() });
                }
            }
            if let Some(c) = json
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_str())
            {
                if !c.is_empty() {
                    full.push_str(c);
                    let _ = on_event.send(ChatEvent::Chunk { content: c.to_string() });
                }
            }
            if json.get("done").and_then(|d| d.as_bool()).unwrap_or(false) {
                let _ = on_event.send(ChatEvent::Done);
            }
        }
    };

    // Ollama шлёт NDJSON. Копим БАЙТЫ (чтобы не резать UTF-8 на границе чанка) и
    // разбираем только целые строки; остаток без \n дочитываем после конца потока.
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
        if cancel.0.load(Ordering::Relaxed) {
            break; // нажали «Стоп» — перестаём читать, соединение закроется и Ollama остановит генерацию
        }
        buf.extend_from_slice(&chunk);
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line_bytes: Vec<u8> = buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line_bytes);
            let line = line.trim();
            if !line.is_empty() {
                handle(line, &mut full);
            }
        }
    }
    // Хвост без завершающего перевода строки — здесь финальные токены ответа.
    if !buf.is_empty() {
        let line = String::from_utf8_lossy(&buf);
        let line = line.trim();
        if !line.is_empty() {
            handle(line, &mut full);
        }
    }

    watchdog.abort(); // генерация закончилась — watchdog больше не нужен
    // Если прервал watchdog (а не пользователь) — отдаём точную причину как ошибку.
    if let Some(reason) = wd_reason.lock().unwrap_or_else(|e| e.into_inner()).take() {
        return Err(format!(
            "Запрос остановлен: {reason}. Попробуйте модель полегче или меньший контекст."
        ));
    }
    Ok(full)
}

/// «Стоп»: помечаем текущий стрим на отмену. chat_stream увидит флаг и прервёт
/// чтение — соединение с Ollama закроется, генерация остановится (не жжём GPU).
#[tauri::command]
fn cancel_stream(cancel: tauri::State<'_, CancelFlag>) {
    cancel.0.store(true, Ordering::Relaxed);
}

// ── Установка моделей (операционный слой): /api/pull с прогрессом ─────────────

/// Прогресс установки модели. status — стадия (manifest/downloading/verify/…),
/// completed/total — байты текущего слоя (0/0 у статусных строк без чисел).
#[derive(Clone, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum PullEvent {
    Progress {
        status: String,
        completed: u64,
        total: u64,
    },
    Done,
}

/// Установка модели в Ollama (`POST /api/pull`, stream:true). Тянет модель из
/// интернета (онлайн-провижининг). Поток NDJSON разбираем тем же способом, что и
/// chat_stream; прогресс шлём через Channel; поддерживаем отмену (PullCancelFlag).
/// Только localhost — наружу ходит сама Ollama, не приложение.
#[tauri::command]
async fn pull_model(
    name: String,
    on_event: Channel<PullEvent>,
    cancel: tauri::State<'_, PullCancelFlag>,
) -> Result<(), String> {
    cancel.0.store(false, Ordering::Relaxed); // новая установка — сбрасываем отмену
    let client = reqwest::Client::new();
    let body = serde_json::json!({ "name": name, "stream": true });

    let mut resp = client
        .post("http://127.0.0.1:11434/api/pull")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Не удалось подключиться к Ollama: {e}"))?;

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
    // дочитка хвоста). Проверяем отмену на каждом чанке — прерывание скачивания.
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
        if cancel.0.load(Ordering::Relaxed) {
            // отмена: рвём соединение; Ollama останавливает закачку и докачает с места при повторе
            return Ok(());
        }
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

    let _ = on_event.send(PullEvent::Done);
    Ok(())
}

/// Отмена текущей установки модели. pull_model увидит флаг и прервёт скачивание.
#[tauri::command]
fn cancel_pull(cancel: tauri::State<'_, PullCancelFlag>) {
    cancel.0.store(true, Ordering::Relaxed);
}

/// Модель + поддержка рассуждений ("thinking") и зрения ("vision") из capabilities.
#[derive(Serialize)]
struct ModelInfo {
    name: String,
    thinking: bool,
    vision: bool,
}

/// Список установленных моделей из Ollama: GET 127.0.0.1:11434/api/tags.
/// Возвращаем имя и поддержку рассуждений (по полю capabilities). Только localhost.
#[tauri::command]
async fn list_models() -> Result<Vec<ModelInfo>, String> {
    let client = reqwest::Client::new();
    let resp = client
        .get("http://127.0.0.1:11434/api/tags")
        .send()
        .await
        .map_err(|e| format!("Не удалось подключиться к Ollama: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("Ollama вернул ошибку {}", resp.status()));
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
                    Some(ModelInfo {
                        name,
                        thinking: has("thinking"),
                        vision: has("vision"),
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
struct ModelSpec {
    tag: &'static str,    // точный тег Ollama (по нему идут pull и сравнение digest)
    role: &'static str,   // "text" | "embed" | "vision" | "code"
    title: &'static str,  // человекочитаемое название
    required: bool,       // обязательная (без неё ломается сценарий) или опциональная
}

/// Нужный набор моделей приложения. Расширяется правкой ОДНОГО этого списка.
/// Теги — реальные (фактически устанавливаемые), не абстрактные.
const MODEL_SET: &[ModelSpec] = &[
    ModelSpec { tag: "qwen3.5:9b", role: "text", title: "Базовая текстовая (чат)", required: true },
    ModelSpec { tag: "qwen3:8b", role: "text", title: "Текстовая (лёгкая)", required: false },
    ModelSpec { tag: "bge-m3:latest", role: "embed", title: "Поиск по документам (RAG)", required: true },
    ModelSpec { tag: "qwen2.5vl:7b", role: "vision", title: "Зрение и OCR", required: false },
    ModelSpec { tag: "moondream:latest", role: "vision", title: "Зрение (лёгкая)", required: false },
    ModelSpec { tag: "qwen2.5-coder:7b-instruct", role: "code", title: "Код", required: false },
];

/// Локальное состояние одной модели набора (без сети). digest/size/date — если установлена.
#[derive(Serialize)]
struct ModelState {
    tag: String,
    role: String,
    title: String,
    required: bool,
    installed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    digest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    modified_at: Option<String>,
}

#[derive(Serialize)]
struct ModelStatesResult {
    models: Vec<ModelState>, // модели набора (в порядке списка)
    others: Vec<String>,     // установленные вне набора — «прочие», не мешаем
}

/// Локальные состояния моделей набора по `/api/tags` (без сети): установлена ли,
/// локальный digest/размер/дата. Статус обновления считается отдельно (M2, онлайн).
#[tauri::command]
async fn model_states() -> Result<ModelStatesResult, String> {
    let client = reqwest::Client::new();
    let resp = client
        .get("http://127.0.0.1:11434/api/tags")
        .send()
        .await
        .map_err(|e| format!("Не удалось подключиться к Ollama: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("Ollama вернул ошибку {}", resp.status()));
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
/// status: "current" (актуальна) | "update" (есть обновление) | "not_installed" | "error".
#[derive(Serialize)]
struct UpdateStatus {
    tag: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

/// Локальные digest по тегам из `/api/tags` (без сети).
async fn local_digests(client: &reqwest::Client) -> Result<std::collections::HashMap<String, String>, String> {
    let resp = client
        .get("http://127.0.0.1:11434/api/tags")
        .send()
        .await
        .map_err(|e| format!("Не удалось подключиться к Ollama: {e}"))?;
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
async fn registry_digest(client: &reqwest::Client, tag: &str) -> Result<String, String> {
    let (model, t) = tag.split_once(':').unwrap_or((tag, "latest"));
    let url = format!("https://registry.ollama.ai/v2/library/{model}/manifests/{t}");
    let resp = client
        .get(&url)
        .header("Accept", "application/vnd.docker.distribution.manifest.v2+json")
        .timeout(std::time::Duration::from_secs(15))
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
async fn check_model_updates() -> Result<Vec<UpdateStatus>, String> {
    let client = reqwest::Client::new();
    let local = local_digests(&client).await?;
    let mut out = Vec::new();
    for spec in MODEL_SET {
        let Some(local_dig) = local.get(spec.tag) else {
            out.push(UpdateStatus { tag: spec.tag.to_string(), status: "not_installed".into(), message: None });
            continue;
        };
        match registry_digest(&client, spec.tag).await {
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

// ── Стабильность (S1): оценка памяти по формуле (вес + KV + буфер + запас) ─────

const GB: f64 = 1024.0 * 1024.0 * 1024.0;
/// Запас под ОС и прочие процессы — отделяет «помещается» от «впритык». Консервативно.
const MEM_SAFETY_GB: f64 = 1.5;
/// Байт на элемент KV-кэша при q8_0 (экономный режим). f16 был бы 2.0.
const KV_BYTES_Q8: f64 = 1.1;
/// Compute-буфер (с flash attention почти не растёт с контекстом).
const COMPUTE_BUF_GB: f64 = 0.6;

/// Доступная сейчас оперативная память, ГБ (кроссплатформенно, sysinfo).
fn available_ram_gb() -> f64 {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    sys.available_memory() as f64 / GB
}

/// Всего оперативной памяти, ГБ.
fn total_ram_gb() -> f64 {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    sys.total_memory() as f64 / GB
}

/// Реалистично доступная под модель память, ГБ. На macOS «available» резко
/// недосчитывает (агрессивный файловый кэш реклеймится под новые аллокации), поэтому
/// берём пол ~55% от total; на Linux/Windows доверяем точному available.
fn usable_ram_gb() -> f64 {
    #[cfg(target_os = "macos")]
    {
        available_ram_gb().max(total_ram_gb() * 0.55)
    }
    #[cfg(not(target_os = "macos"))]
    {
        available_ram_gb()
    }
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
}

/// Оценить, поместится ли модель при заданном контексте: вес (size из /api/tags) +
/// KV-кэш (метаданные /api/show × num_ctx) + compute-буфер + запас, против доступной
/// памяти. На дискретном GPU часть сверх VRAM уходит в RAM (своп — главный риск);
/// на унифицированной памяти (Apple, VRAM=None) всё в общей RAM. Без блокировки.
async fn estimate(model: &str, num_ctx: u64) -> Result<MemoryEstimate, String> {
    let client = reqwest::Client::new();

    // Что сейчас в памяти (/api/ps) — первым: целевая уже загружена → помещается (хот-
    // путь, без тяжёлого /api/show). Прочие при MAX_LOADED_MODELS=1 выгрузятся под
    // новую → их размер прибавляется к доступному.
    let mut loaded_total: u64 = 0;
    let mut already_loaded = false;
    if let Ok(resp) = client.get("http://127.0.0.1:11434/api/ps").send().await {
        if let Ok(ps) = resp.json::<serde_json::Value>().await {
            if let Some(arr) = ps.get("models").and_then(|m| m.as_array()) {
                for m in arr {
                    let name = m.get("name").and_then(|n| n.as_str());
                    let size = m.get("size").and_then(|s| s.as_u64()).unwrap_or(0);
                    if name == Some(model) {
                        already_loaded = true;
                    } else {
                        loaded_total += size;
                    }
                }
            }
        }
    }
    let available_gb = usable_ram_gb() + loaded_total as f64 / GB;
    if already_loaded {
        return Ok(MemoryEstimate {
            fits: "ok".into(),
            limiting: "ram".into(),
            required_gb: 0.0,
            available_gb,
            weight_gb: 0.0,
            kv_gb: 0.0,
        });
    }

    // вес = размер GGUF из /api/tags
    let tags: serde_json::Value = client
        .get("http://127.0.0.1:11434/api/tags")
        .send()
        .await
        .map_err(|e| format!("Ollama недоступна: {e}"))?
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
    let show: serde_json::Value = client
        .post("http://127.0.0.1:11434/api/show")
        .json(&serde_json::json!({ "model": model }))
        .send()
        .await
        .map_err(|e| format!("Ollama недоступна: {e}"))?
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
            elems as f64 * KV_BYTES_Q8 / GB
        }
        // метаданных нет → консервативная оценка KV как доля веса под контекст
        _ => weight_gb * 0.2 * (num_ctx as f64 / 8192.0),
    };

    let required_gb = weight_gb + kv_gb + COMPUTE_BUF_GB + MEM_SAFETY_GB;

    // Что сейчас в памяти (/api/ps): целевая модель уже загружена → помещается (её
    // объём уже учтён). Прочие при MAX_LOADED_MODELS=1 выгрузятся под новую → их
    // размер освобождается, поэтому прибавляем к доступному.
    let mut loaded_total: u64 = 0;
    let mut already_loaded = false;
    if let Ok(resp) = client.get("http://127.0.0.1:11434/api/ps").send().await {
        if let Ok(ps) = resp.json::<serde_json::Value>().await {
            if let Some(arr) = ps.get("models").and_then(|m| m.as_array()) {
                for m in arr {
                    let name = m.get("name").and_then(|n| n.as_str());
                    let size = m.get("size").and_then(|s| s.as_u64()).unwrap_or(0);
                    if name == Some(model) {
                        already_loaded = true;
                    } else {
                        loaded_total += size;
                    }
                }
            }
        }
    }

    let (vram_gb, _) = detect_vram();
    // эффективно доступно = реалистично доступная RAM + то, что выгрузится при загрузке новой
    let available_gb = usable_ram_gb() + loaded_total as f64 / GB;

    // сколько потребуется именно от RAM (своп — триггер зависания); на дискретном
    // GPU часть до объёма VRAM не давит на RAM.
    let (ram_needed, limiting) = match vram_gb {
        Some(vram) => ((required_gb - vram).max(0.0), "vram"),
        None => (required_gb, "ram"),
    };

    let fits = if already_loaded || ram_needed <= available_gb {
        "ok"
    } else if ram_needed - MEM_SAFETY_GB <= available_gb {
        "tight"
    } else {
        "no"
    };

    Ok(MemoryEstimate {
        fits: fits.into(),
        limiting: limiting.into(),
        required_gb,
        available_gb,
        weight_gb,
        kv_gb,
    })
}

/// Команда: оценить память модели при контексте (для блока «Железо»/диагностики).
#[tauri::command]
async fn estimate_memory(model: String, num_ctx: u64) -> Result<MemoryEstimate, String> {
    estimate(&model, num_ctx).await
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

/// Размеры установленных моделей (имя → байты) из /api/tags — для подбора «полегче».
async fn installed_sizes() -> std::collections::HashMap<String, u64> {
    let client = reqwest::Client::new();
    let mut out = std::collections::HashMap::new();
    if let Ok(resp) = client.get("http://127.0.0.1:11434/api/tags").send().await {
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

/// Решение по запросу: что и с каким контекстом запускать.
#[derive(Serialize)]
struct InferencePlan {
    action: String,         // "ok" | "downscale" | "refuse"
    model: String,          // модель для запроса
    num_ctx: u64,           // контекст для запроса
    reason: Option<String>, // пояснение для downscale/refuse
    original_model: String, // что просил пользователь
}

/// Лестница смягчения ПЕРЕД запуском (S2): экономрежим (K) уже применён → пробуем
/// снизить контекст у запрошенной модели → если не помещается, подбираем более лёгкую
/// модель той же роли «вниз» → если ничего, честный отказ с дефицитом. Только «вниз»;
/// ручной выбор не меняется (downscale действует лишь на этот запрос).
#[tauri::command]
async fn plan_inference(model: String) -> Result<InferencePlan, String> {
    // 1) запрошенная модель: пробуем снижать контекст
    for &ctx in &CTX_LADDER {
        let est = estimate(&model, ctx).await?;
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
                let est = estimate(cand.tag, ctx).await?;
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
                    });
                }
            }
        }
    }

    // 3) ничего не помещается → честный отказ с дефицитом
    let last = *CTX_LADDER.last().unwrap();
    let est = estimate(&model, last).await?;
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
    })
}

/// Версия Ollama: GET 127.0.0.1:11434/api/version — мягкая проверка «движок жив».
/// Таймаут 3 с, чтобы не зависнуть, если порт открыт, но ответа нет. Только localhost.
#[tauri::command]
async fn ollama_version() -> Result<String, String> {
    let client = reqwest::Client::new();
    let resp = client
        .get("http://127.0.0.1:11434/api/version")
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await
        .map_err(|e| format!("Не удалось подключиться к Ollama: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("Ollama вернул ошибку {}", resp.status()));
    }

    let json: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    let version = json
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("неизвестно")
        .to_string();
    Ok(version)
}

/// Установлена ли модель эмбеддингов (для интерфейса базы документов): можно ли
/// индексировать и искать. Мягкая проверка, сетевые сбои → false.
#[tauri::command]
async fn embedding_status() -> bool {
    embed::is_available().await
}

/// Override-путь из настроек (для гибкого разрешения путей движка/моделей). Пустой
/// или отсутствующий → None. Заполняется в будущем офлайн-инсталлером.
fn read_setting_path(app: &tauri::AppHandle, key: &str) -> Option<std::path::PathBuf> {
    read_settings(app)
        .get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from)
}

/// Обеспечить работу движка Ollama при старте: переиспользовать запущенный или
/// поднять свой (с OLLAMA_HOST/OLLAMA_MODELS), дождавшись готовности. Пути к движку
/// и моделям — гибко (override из настроек → ресурс рядом → PATH), без абсолютов.
#[tauri::command]
async fn ensure_engine(
    app: tauri::AppHandle,
    state: tauri::State<'_, engine::EngineState>,
) -> Result<engine::EngineStatus, String> {
    let exe_override = read_setting_path(&app, "ollama_path");
    let models_override = read_setting_path(&app, "ollama_models_dir");
    let resource_dir = app.path().resource_dir().ok();
    Ok(engine::ensure(&state, exe_override, models_override, resource_dir).await)
}

/// Сведения о железе для «светофора». Считается локально, без сети.
#[derive(Serialize)]
struct HardwareInfo {
    ram_gb: f64,
    cpu_cores: usize,
    vram_gb: Option<f64>,
    vram_source: String, // "dxgi" | "nvidia-smi" | "rocm-smi" | "unified" | "unknown"
    tier: String,        // "green" | "yellow" | "red"
}

/// Детект ресурсов машины. Команда синхронная: внутри блокирующие вызовы
/// (sysinfo, запуск утилит), а Tauri выполняет sync-команды в пуле потоков —
/// главный поток не блокируется. Только локальные системные вызовы, без сети.
#[tauri::command]
fn detect_hardware() -> Result<HardwareInfo, String> {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    let ram_gb = sys.total_memory() as f64 / 1024.0 / 1024.0 / 1024.0;
    let cpu_cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(0);

    let (vram_gb, vram_source) = detect_vram();

    // Пороги «светофора» (по требованию): VRAM важнее, но RAM тоже учитываем.
    let has_vram = |min: f64| vram_gb.map_or(false, |v| v >= min);
    let tier = if has_vram(6.0) || ram_gb >= 16.0 {
        "green"
    } else if has_vram(4.0) || ram_gb >= 8.0 {
        "yellow"
    } else {
        "red"
    }
    .to_string();

    Ok(HardwareInfo {
        ram_gb,
        cpu_cores,
        vram_gb,
        vram_source,
        tier,
    })
}

// macOS: общая (unified) память — отдельной VRAM нет, уровень считаем по RAM.
#[cfg(target_os = "macos")]
fn detect_vram() -> (Option<f64>, String) {
    (None, "unified".to_string())
}

// Windows: DXGI — поле DedicatedVideoMemory. Намеренно НЕ WMI AdapterRAM
// (он режет видеопамять до 4 ГБ). Берём максимум по адаптерам.
#[cfg(target_os = "windows")]
fn detect_vram() -> (Option<f64>, String) {
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, IDXGIFactory1, DXGI_ADAPTER_DESC, DXGI_ERROR_NOT_FOUND,
    };
    unsafe {
        let factory: IDXGIFactory1 = match CreateDXGIFactory1() {
            Ok(f) => f,
            Err(_) => return (None, "unknown".to_string()),
        };
        let mut best: u64 = 0;
        let mut i = 0u32;
        loop {
            match factory.EnumAdapters(i) {
                Ok(adapter) => {
                    let mut desc = DXGI_ADAPTER_DESC::default();
                    if adapter.GetDesc(&mut desc).is_ok() {
                        best = best.max(desc.DedicatedVideoMemory as u64);
                    }
                    i += 1;
                }
                Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(_) => break,
            }
        }
        if best > 0 {
            (Some(best as f64 / 1024.0 / 1024.0 / 1024.0), "dxgi".to_string())
        } else {
            (None, "unknown".to_string())
        }
    }
}

// Linux: nvidia-smi → rocm-smi → неизвестно. Утилиты могут отсутствовать —
// ошибки гасим (не паникуем), сети нет.
#[cfg(target_os = "linux")]
fn detect_vram() -> (Option<f64>, String) {
    use std::process::Command;

    // NVIDIA: вывод в МиБ, по строке на GPU — берём максимум.
    if let Ok(out) = Command::new("nvidia-smi")
        .args(["--query-gpu=memory.total", "--format=csv,noheader,nounits"])
        .output()
    {
        if out.status.success() {
            let max_mib = String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter_map(|l| l.trim().parse::<f64>().ok())
                .fold(0.0_f64, f64::max);
            if max_mib > 0.0 {
                return (Some(max_mib / 1024.0), "nvidia-smi".to_string());
            }
        }
    }

    // AMD: rocm-smi --showmeminfo vram --csv. Эвристика: наибольшее число байт
    // в выводе = суммарная VRAM.
    if let Ok(out) = Command::new("rocm-smi")
        .args(["--showmeminfo", "vram", "--csv"])
        .output()
    {
        if out.status.success() {
            let max_bytes = String::from_utf8_lossy(&out.stdout)
                .split(|c: char| !c.is_ascii_digit())
                .filter_map(|t| t.parse::<u64>().ok())
                .max()
                .unwrap_or(0);
            if max_bytes > 0 {
                return (
                    Some(max_bytes as f64 / 1024.0 / 1024.0 / 1024.0),
                    "rocm-smi".to_string(),
                );
            }
        }
    }

    (None, "unknown".to_string())
}

// Прочие ОС: видеопамять неизвестна, ориентируемся на RAM.
#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
fn detect_vram() -> (Option<f64>, String) {
    (None, "unknown".to_string())
}

// ── Документы (Фаза A): извлечение текста из одного файла ────────────────────

/// Результат извлечения текста из документа.
#[derive(Serialize)]
struct DocumentText {
    name: String,
    ext: String,
    text: String,
    chars: usize,
}

/// Извлекает текст из файла по пути. Тип — по расширению. Чтение только в Rust.
/// Синхронная команда: тяжёлый разбор (крупный PDF) идёт в пуле потоков Tauri.
#[tauri::command]
fn extract_document(path: String) -> Result<DocumentText, String> {
    read_document(&path)
}

/// Читает изображение и возвращает СЫРОЙ base64 (без префикса `data:`) для вложения
/// в сообщение (поле `images` у vision-моделей). Проверяет тип и размер; чтение — в Rust.
#[tauri::command]
fn read_image_base64(path: String) -> Result<String, String> {
    use base64::Engine;
    let p = std::path::Path::new(&path);
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .unwrap_or_default();
    if !matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "webp" | "gif") {
        return Err("Формат не поддерживается. Доступны: PNG, JPG, WEBP, GIF".to_string());
    }
    let bytes = std::fs::read(p).map_err(|e| format!("Не удалось прочитать изображение: {e}"))?;
    if bytes.is_empty() {
        return Err("Файл пуст".to_string());
    }
    // Картинка занимает контекст vision-модели — ограничиваем размер во избежание
    // переполнения num_ctx и тяжёлых запросов. Очень большие — понятная ошибка.
    const MAX: usize = 12 * 1024 * 1024;
    if bytes.len() > MAX {
        return Err(format!(
            "Слишком большое изображение ({} МБ). Максимум — 12 МБ; уменьшите файл.",
            bytes.len() / 1024 / 1024
        ));
    }
    Ok(base64::engine::general_purpose::STANDARD.encode(&bytes))
}

/// Извлечение текста из файла. Переиспользуется индексацией базы (Фаза B3),
/// поэтому вынесено из команды в обычную функцию.
fn read_document(path: &str) -> Result<DocumentText, String> {
    let p = std::path::Path::new(path);
    if !p.is_file() {
        return Err("Файл не найден или недоступен".to_string());
    }
    let name = p
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("документ")
        .to_string();
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .unwrap_or_default();

    let text = match ext.as_str() {
        "txt" | "md" => extract_txt(p)?,
        "pdf" => extract_pdf(p)?,
        "docx" => extract_docx(p)?,
        _ => return Err("Формат не поддерживается. Доступны: PDF, Word, текст".to_string()),
    };

    let text = text.trim().to_string();
    if text.chars().filter(|c| !c.is_whitespace()).count() < 5 {
        return Err("Не удалось извлечь текст: файл пуст или без текстового слоя".to_string());
    }
    let chars = text.chars().count();
    Ok(DocumentText { name, ext, text, chars })
}

// TXT/MD: читаем как UTF-8 с заменой битых байтов (без паники).
fn extract_txt(p: &std::path::Path) -> Result<String, String> {
    let bytes = std::fs::read(p).map_err(|e| format!("Не удалось прочитать файл: {e}"))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

// PDF: текстовый слой через pdf-extract (lopdf, чистый Rust). Скан → понятная ошибка.
fn extract_pdf(p: &std::path::Path) -> Result<String, String> {
    let bytes = std::fs::read(p).map_err(|e| format!("Не удалось прочитать файл: {e}"))?;
    let text = pdf_extract::extract_text_from_mem(&bytes)
        .map_err(|e| format!("Не удалось разобрать PDF: {e}"))?;
    if text.chars().filter(|c| !c.is_whitespace()).count() < 20 {
        return Err(
            "Похоже, это PDF-скан (изображение без текстового слоя). \
             Распознавание текста (OCR) появится в этапе по зрению."
                .to_string(),
        );
    }
    Ok(text)
}

// DOCX: это ZIP; берём word/document.xml и собираем текст из <w:t>, перенос на </w:p>.
fn extract_docx(p: &std::path::Path) -> Result<String, String> {
    use quick_xml::events::Event;
    use std::io::Read;

    let file = std::fs::File::open(p).map_err(|e| format!("Не удалось открыть файл: {e}"))?;
    let mut zip = zip::ZipArchive::new(file).map_err(|_| "Повреждённый файл DOCX".to_string())?;
    let mut xml = String::new();
    zip.by_name("word/document.xml")
        .map_err(|_| "Это не похоже на документ Word".to_string())?
        .read_to_string(&mut xml)
        .map_err(|e| format!("Ошибка чтения DOCX: {e}"))?;

    let mut reader = quick_xml::Reader::from_str(&xml);
    let mut out = String::new();
    let mut in_text = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) if e.name().as_ref() == b"w:t" => in_text = true,
            Ok(Event::End(e)) if e.name().as_ref() == b"w:t" => in_text = false,
            Ok(Event::End(e)) if e.name().as_ref() == b"w:p" => out.push('\n'),
            Ok(Event::Text(t)) if in_text => {
                if let Ok(decoded) = t.decode() {
                    match quick_xml::escape::unescape(&decoded) {
                        Ok(u) => out.push_str(&u),
                        Err(_) => out.push_str(&decoded),
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("Ошибка разбора DOCX: {e}")),
            _ => {}
        }
    }
    Ok(out)
}

// ── База документов (Фаза B): индексация, список, удаление ────────────────────

/// Прогресс индексации, шлётся в окно через Channel (как стриминг чата).
/// phase: "chunk" | "embed" | "store" | "done".
#[derive(Clone, Serialize)]
struct IndexProgress {
    phase: String,
    current: usize,
    total: usize,
}

/// Итог индексации одного документа.
/// status: "indexed" — добавлен; "exists" — уже был (тот же sha256), не дублируем;
/// "reindexed" — база была пересоздана из-за смены размерности вектора.
#[derive(Serialize)]
struct IndexResult {
    status: String,
    document: docstore::DocumentMeta,
    rebuilt: bool,
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Индексация документа в базу: sha256 → извлечение текста (Фаза A) → чанкинг →
/// эмбеддинги батчами (с прогрессом) → запись в транзакции. Идемпотентно по sha256.
#[tauri::command]
async fn index_document(
    app: tauri::AppHandle,
    path: String,
    on_progress: Channel<IndexProgress>,
) -> Result<IndexResult, String> {
    // 1) sha256 содержимого файла — для идемпотентности.
    let bytes = std::fs::read(&path).map_err(|e| format!("Не удалось прочитать файл: {e}"))?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let sha: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();

    // 2) текст (переиспуем извлечение Фазы A) + резка на фрагменты по границам.
    let doc = read_document(&path)?;
    let chunks = chunk::chunk_text(&doc.text);
    if chunks.is_empty() {
        return Err("Не удалось разбить документ на фрагменты".to_string());
    }
    let total = chunks.len();
    let _ = on_progress.send(IndexProgress {
        phase: "chunk".into(),
        current: total,
        total,
    });

    // 3) дубликат по sha256 — короткая сессия БД, соединение не держим через await.
    {
        let conn = docstore::open(&docstore::db_path(&app)?)?;
        if let Some(existing) = docstore::find_by_hash(&conn, &sha)? {
            return Ok(IndexResult {
                status: "exists".into(),
                document: existing,
                rebuilt: false,
            });
        }
    }

    // 4) эмбеддинги батчами — самый долгий этап, шлём прогресс по каждому батчу.
    const BATCH: usize = 16;
    let mut vectors: Vec<Vec<f32>> = Vec::with_capacity(total);
    for batch in chunks.chunks(BATCH) {
        let part = embed::embed_batch(batch).await?;
        vectors.extend(part);
        let _ = on_progress.send(IndexProgress {
            phase: "embed".into(),
            current: vectors.len(),
            total,
        });
    }
    let dim = vectors[0].len();

    // 5) запись: документ + фрагменты + векторы одной транзакцией (атомарно).
    let added_at = now_ms();
    let mut conn = docstore::open(&docstore::db_path(&app)?)?;
    let rebuilt = docstore::ensure_vec_table(&conn, dim)?;
    let tx = conn
        .transaction()
        .map_err(|e| format!("Не удалось начать транзакцию: {e}"))?;
    let doc_id = docstore::insert_document(
        &tx,
        &doc.name,
        &doc.ext,
        &sha,
        added_at,
        doc.chars as i64,
        total as i64,
    )?;
    for (i, (text, vec)) in chunks.iter().zip(&vectors).enumerate() {
        let chunk_id = docstore::insert_chunk(&tx, doc_id, i as i64, text, None)?;
        docstore::insert_vector(&tx, chunk_id, vec)?;
    }
    tx.commit()
        .map_err(|e| format!("Не удалось сохранить индекс: {e}"))?;
    let _ = on_progress.send(IndexProgress {
        phase: "done".into(),
        current: total,
        total,
    });

    Ok(IndexResult {
        status: if rebuilt { "reindexed" } else { "indexed" }.into(),
        document: docstore::DocumentMeta {
            id: doc_id,
            filename: doc.name,
            ext: doc.ext,
            added_at,
            char_count: doc.chars as i64,
            chunk_count: total as i64,
        },
        rebuilt,
    })
}

/// Список документов базы (для интерфейса).
#[tauri::command]
fn list_documents(app: tauri::AppHandle) -> Result<Vec<docstore::DocumentMeta>, String> {
    let conn = docstore::open(&docstore::db_path(&app)?)?;
    docstore::list_documents(&conn)
}

/// Удаление документа из базы (вместе с фрагментами и векторами).
#[tauri::command]
fn delete_document(app: tauri::AppHandle, id: i64) -> Result<(), String> {
    let conn = docstore::open(&docstore::db_path(&app)?)?;
    docstore::delete_document(&conn, id)
}

/// Пуста ли база документов (фронт решает, включать ли поиск перед вопросом).
#[tauri::command]
fn documents_empty(app: tauri::AppHandle) -> Result<bool, String> {
    let conn = docstore::open(&docstore::db_path(&app)?)?;
    docstore::is_empty(&conn)
}

/// Семантический поиск по базе: эмбеддинг вопроса → KNN top-k фрагментов с
/// метаданными. Пустой запрос/пустая база → пусто (без обращения к эмбеддингам).
#[tauri::command]
async fn search_documents(
    app: tauri::AppHandle,
    query: String,
    k: usize,
) -> Result<Vec<docstore::RetrievedChunk>, String> {
    let q = query.trim();
    if q.is_empty() {
        return Ok(Vec::new());
    }
    // пустая база → не трогаем эмбеддинги (соединение не держим через await)
    {
        let conn = docstore::open(&docstore::db_path(&app)?)?;
        if docstore::is_empty(&conn)? {
            return Ok(Vec::new());
        }
    }
    let qvec = embed::embed_one(q).await?;
    let conn = docstore::open(&docstore::db_path(&app)?)?;
    docstore::search(&conn, &qvec, k)
}

// ── История диалогов: хранение в appDataDir/conversations/<id>.json ──────────

/// Полный диалог (как лежит в файле).
#[derive(Serialize, Deserialize)]
struct Conversation {
    id: String,
    title: String,
    updated_at: i64,
    messages: Vec<ChatMessage>,
}

/// Краткая карточка диалога для списка в боковой панели.
#[derive(Serialize)]
struct ConversationMeta {
    id: String,
    title: String,
    updated_at: i64,
}

/// Каталог с диалогами внутри appDataDir (кроссплатформенно). Создаём при необходимости.
fn conversations_dir(app: &tauri::AppHandle) -> Result<std::path::PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Не удалось получить appDataDir: {e}"))?
        .join("conversations");
    std::fs::create_dir_all(&dir).map_err(|e| format!("Не удалось создать каталог: {e}"))?;
    Ok(dir)
}

/// Атомарная запись: пишем во временный файл и переименовываем. При падении в
/// момент записи исходный файл не повреждается (важно для истории и настроек).
fn write_atomic(path: &std::path::Path, content: &str) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, content).map_err(|e| format!("Не удалось записать файл: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("Не удалось сохранить файл: {e}"))
}

/// Защита от обхода путей: id формируем сами (UUID), но всё равно проверяем.
fn validate_id(id: &str) -> Result<(), String> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err("Некорректный идентификатор диалога".to_string());
    }
    Ok(())
}

#[tauri::command]
fn list_conversations(app: tauri::AppHandle) -> Result<Vec<ConversationMeta>, String> {
    let dir = conversations_dir(&app)?;
    let mut metas: Vec<ConversationMeta> = Vec::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Ok(metas), // каталога нет/пуст — пустой список, не ошибка
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(conv) = serde_json::from_str::<Conversation>(&text) {
                metas.push(ConversationMeta {
                    id: conv.id,
                    title: conv.title,
                    updated_at: conv.updated_at,
                });
            }
        }
    }
    metas.sort_by(|a, b| b.updated_at.cmp(&a.updated_at)); // свежие сверху
    Ok(metas)
}

#[tauri::command]
fn load_conversation(app: tauri::AppHandle, id: String) -> Result<Conversation, String> {
    validate_id(&id)?;
    let path = conversations_dir(&app)?.join(format!("{id}.json"));
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("Не удалось прочитать диалог: {e}"))?;
    serde_json::from_str::<Conversation>(&text)
        .map_err(|e| format!("Повреждённый файл диалога: {e}"))
}

#[tauri::command]
fn save_conversation(app: tauri::AppHandle, conversation: Conversation) -> Result<(), String> {
    validate_id(&conversation.id)?;
    let path = conversations_dir(&app)?.join(format!("{}.json", conversation.id));
    let text = serde_json::to_string_pretty(&conversation).map_err(|e| e.to_string())?;
    write_atomic(&path, &text)
}

#[tauri::command]
fn delete_conversation(app: tauri::AppHandle, id: String) -> Result<(), String> {
    validate_id(&id)?;
    let path = conversations_dir(&app)?.join(format!("{id}.json"));
    if path.exists() {
        std::fs::remove_file(&path).map_err(|e| format!("Не удалось удалить диалог: {e}"))?;
    }
    Ok(())
}

/// Очистка всех диалогов: удаляем все *.json в каталоге conversations.
#[tauri::command]
fn clear_conversations(app: tauri::AppHandle) -> Result<(), String> {
    let dir = conversations_dir(&app)?;
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    Ok(())
}

// ── Настройки приложения: appDataDir/settings.json (объект ключ→строка) ──────

fn settings_path(app: &tauri::AppHandle) -> Result<std::path::PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Не удалось получить appDataDir: {e}"))?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("Не удалось создать каталог: {e}"))?;
    Ok(dir.join("settings.json"))
}

fn read_settings(app: &tauri::AppHandle) -> serde_json::Map<String, serde_json::Value> {
    if let Ok(path) = settings_path(app) {
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(serde_json::Value::Object(map)) = serde_json::from_str(&text) {
                return map;
            }
        }
    }
    serde_json::Map::new()
}

#[tauri::command]
fn get_setting(app: tauri::AppHandle, key: String) -> Result<Option<String>, String> {
    let map = read_settings(&app);
    Ok(map.get(&key).and_then(|v| v.as_str()).map(String::from))
}

/// Запись одной настройки под мьютексом (общий помощник для set_setting и
/// команд override-путей). Лок на весь read-modify-write: параллельная запись
/// другого ключа не затрёт наш. into_inner — восстановление после «отравления».
fn write_setting(
    app: &tauri::AppHandle,
    lock: &SettingsLock,
    key: &str,
    value: &str,
) -> Result<(), String> {
    let _guard = lock.0.lock().unwrap_or_else(|e| e.into_inner());
    let mut map = read_settings(app);
    map.insert(key.to_string(), serde_json::Value::String(value.to_string()));
    let path = settings_path(app)?;
    let text = serde_json::to_string_pretty(&serde_json::Value::Object(map))
        .map_err(|e| e.to_string())?;
    write_atomic(&path, &text)
}

#[tauri::command]
fn set_setting(
    app: tauri::AppHandle,
    lock: tauri::State<'_, SettingsLock>,
    key: String,
    value: String,
) -> Result<(), String> {
    write_setting(&app, &lock, &key, &value)
}

// ── Офлайн-поставка: запись override-путей движка/моделей (Фаза упаковки) ─────
// Пишем в те же ключи, что читает ensure_engine (ollama_path / ollama_models_dir).

/// Задать путь к исполняемому файлу движка (для air-gapped, когда Ollama не в PATH).
#[tauri::command]
fn set_engine_path(
    app: tauri::AppHandle,
    lock: tauri::State<'_, SettingsLock>,
    path: String,
) -> Result<(), String> {
    engine::validate_engine_exe(std::path::Path::new(&path))?;
    write_setting(&app, &lock, "ollama_path", &path)
}

/// Задать путь к предзагруженному каталогу моделей Ollama (офлайн-поставка моделей).
#[tauri::command]
fn set_models_dir(
    app: tauri::AppHandle,
    lock: tauri::State<'_, SettingsLock>,
    path: String,
) -> Result<(), String> {
    engine::validate_models_dir(std::path::Path::new(&path))?;
    write_setting(&app, &lock, "ollama_models_dir", &path)
}

/// Сбросить override-пути → возврат к цепочке ресурс→PATH (пустые ключи = None).
#[tauri::command]
fn clear_engine_overrides(
    app: tauri::AppHandle,
    lock: tauri::State<'_, SettingsLock>,
) -> Result<(), String> {
    write_setting(&app, &lock, "ollama_path", "")?;
    write_setting(&app, &lock, "ollama_models_dir", "")
}

/// Применить актуальные override к движку: если движок наш — перезапустить его с
/// новым OLLAMA_MODELS; если внешний/системный — честно сообщить, что применится
/// при следующем нашем запуске (чужой экземпляр не трогаем). Без сети (офлайн-путь).
#[tauri::command]
async fn reload_engine(
    app: tauri::AppHandle,
    state: tauri::State<'_, engine::EngineState>,
) -> Result<engine::EngineStatus, String> {
    if engine::was_started_by_us(&state) {
        engine::stop_if_ours(&state); // гасит наш процесс и ждёт освобождения
    } else if engine::is_running().await {
        // системный/внешний движок — не трогаем; override вступит при нашем запуске
        return Ok(engine::EngineStatus {
            status: "external".into(),
            message: "Движок запущен системно — указанный каталог применится, когда \
                      движок поднимет само приложение (при запуске без системной Ollama)."
                .into(),
        });
    }
    let exe_override = read_setting_path(&app, "ollama_path");
    let models_override = read_setting_path(&app, "ollama_models_dir");
    let resource_dir = app.path().resource_dir().ok();
    Ok(engine::ensure(&state, exe_override, models_override, resource_dir).await)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    docstore::register_vec(); // зарегистрировать sqlite-vec до открытия любых соединений
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(CancelFlag(Arc::new(AtomicBool::new(false))))
        .manage(PullCancelFlag(AtomicBool::new(false)))
        .manage(SettingsLock(std::sync::Mutex::new(())))
        .manage(engine::EngineState::new())
        .invoke_handler(tauri::generate_handler![
            chat_stream,
            cancel_stream,
            pull_model,
            cancel_pull,
            ensure_engine,
            extract_document,
            read_image_base64,
            index_document,
            list_documents,
            delete_document,
            documents_empty,
            search_documents,
            list_models,
            model_states,
            check_model_updates,
            estimate_memory,
            plan_inference,
            ollama_version,
            embedding_status,
            detect_hardware,
            list_conversations,
            load_conversation,
            save_conversation,
            delete_conversation,
            clear_conversations,
            get_setting,
            set_setting,
            set_engine_path,
            set_models_dir,
            clear_engine_overrides,
            reload_engine
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|handle, event| {
            // На выходе из приложения — остановить движок, ЕСЛИ его подняли мы сами.
            // Внешний/системный экземпляр не трогаем (Принцип 1).
            if let tauri::RunEvent::Exit = event {
                let state = handle.state::<engine::EngineState>();
                engine::stop_if_ours(&state);
            }
        });
}
