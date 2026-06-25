// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/
use serde::{Deserialize, Serialize};
use tauri::ipc::Channel;
use tauri::Manager;

/// Одно сообщение в диалоге (роль + текст). Приходит с фронтенда.
#[derive(Clone, Serialize, Deserialize)]
struct ChatMessage {
    role: String,
    content: String,
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
    on_event: Channel<ChatEvent>,
) -> Result<String, String> {
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
            // потолок из CLAUDE.md для целевых 6 ГБ VRAM.
            "num_ctx": 8192
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
    Ok(full)
}

/// Модель + флаг поддержки рассуждений (capability "thinking").
#[derive(Serialize)]
struct ModelInfo {
    name: String,
    thinking: bool,
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
                    let thinking = m
                        .get("capabilities")
                        .and_then(|c| c.as_array())
                        .map(|caps| caps.iter().any(|c| c.as_str() == Some("thinking")))
                        .unwrap_or(false);
                    Some(ModelInfo { name, thinking })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(models)
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
    std::fs::write(&path, text).map_err(|e| format!("Не удалось сохранить диалог: {e}"))
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

#[tauri::command]
fn set_setting(app: tauri::AppHandle, key: String, value: String) -> Result<(), String> {
    let mut map = read_settings(&app);
    map.insert(key, serde_json::Value::String(value));
    let path = settings_path(&app)?;
    let text = serde_json::to_string_pretty(&serde_json::Value::Object(map))
        .map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| format!("Не удалось сохранить настройку: {e}"))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            chat_stream,
            list_models,
            ollama_version,
            detect_hardware,
            list_conversations,
            load_conversation,
            save_conversation,
            delete_conversation,
            get_setting,
            set_setting
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
