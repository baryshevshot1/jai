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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![chat_stream])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
