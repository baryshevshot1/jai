// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/
use serde::{Deserialize, Serialize};
use tauri::ipc::Channel;

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
    Done,
}

/// Стриминг ответа от Ollama. Фронт передаёт модель, историю и «канал»
/// `on_event`, в который мы по мере поступления шлём кусочки текста.
/// Весь сетевой трафик — строго на 127.0.0.1:11434 (правило проекта).
#[tauri::command]
async fn chat_stream(
    model: String,
    messages: Vec<ChatMessage>,
    on_event: Channel<ChatEvent>,
) -> Result<(), String> {
    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "model": model,
        "messages": messages,
        "stream": true,
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
    // Копим байты в буфер и разбираем по мере появления переводов строки.
    let mut buffer = String::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(pos) = buffer.find('\n') {
            let line: String = buffer.drain(..=pos).collect();
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            // Битую/неполную строку просто пропускаем — придёт в следующем чанке.
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(line) {
                if let Some(content) = json
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_str())
                {
                    if !content.is_empty() {
                        let _ = on_event.send(ChatEvent::Chunk {
                            content: content.to_string(),
                        });
                    }
                }
                if json.get("done").and_then(|d| d.as_bool()).unwrap_or(false) {
                    let _ = on_event.send(ChatEvent::Done);
                }
            }
        }
    }
    Ok(())
}

/// Список установленных моделей из Ollama: GET 127.0.0.1:11434/api/tags.
/// Возвращаем только имена (поле "name"). Запрос идёт из Rust — правило localhost.
#[tauri::command]
async fn list_models() -> Result<Vec<String>, String> {
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
                .filter_map(|m| m.get("name").and_then(|n| n.as_str()).map(String::from))
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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            chat_stream,
            list_models,
            ollama_version,
            detect_hardware
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
