// ── Чат: стриминг ответа Ollama, «щадящий режим», watchdog и «Стоп» ──────────
// Ядро офлайн-чата: сообщения и события IPC, стрим NDJSON от движка, сторож памяти
// и перегрева. Тот же стрим переиспользует агентный цикл (онлайн-слой, agent.rs).

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::ipc::Channel;

use crate::{tools, with_cancel, CancelFlag, HTTP};

/// Watchdog (S3): период замеров и порог роста свопа, при котором прерываем запрос.
/// Консервативно (≥512 МБ роста = модель реально сливается в своп), чтобы не рубить
/// валидные запросы из-за мелких колебаний. Лёгкий: замер раз в 1.5 с.
const WATCHDOG_INTERVAL_MS: u64 = 1500;
const SWAP_TRIP_BYTES: u64 = 512 * 1024 * 1024;

/// Порог перегрева (°C) и выдержка (число подряд замеров) до мягкой остановки
/// генерации. Порог высокий сознательно: под нагрузкой 85–95° — штатная работа
/// современных CPU, аварийное выключение ОС начинается ~105° — рубим только
/// устойчивый выход за 97°. Датчики: Linux (hwmon) — да, macOS — частично,
/// Windows sysinfo температур не отдаёт (там защита остаётся по свопу).
const TEMP_TRIP_C: f32 = 97.0;
const TEMP_TRIP_TICKS: u32 = 3; // 3 × 1.5 с ≈ 5 секунд устойчивого перегрева

/// Число потоков генерации в «щадящем режиме»: половина ядер, минимум 2 —
/// нейросеть не выедает все ядра, системе и охлаждению остаётся запас.
fn gentle_threads() -> u64 {
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4) as u64;
    (cores / 2).max(2)
}

/// Применить «щадящий режим» к телу запроса /api/chat: ограничить потоки CPU.
/// Пер-запросная опция — действует на любой движок (наш или системный).
pub(crate) fn apply_gentle(body: &mut serde_json::Value, gentle: Option<bool>) {
    if gentle.unwrap_or(false) {
        body["options"]["num_thread"] = serde_json::json!(gentle_threads());
    }
}

/// Документ, прикреплённый к сообщению пользователя (для сохранения в истории).
/// Текст уже усечён фронтендом под бюджет контекста. В Ollama НЕ уходит —
/// при отправке фронт собирает чистые {role, content}, поэтому здесь doc = None.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct DocAttachment {
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
pub(crate) struct SourceRef {
    filename: String,
    chunk_index: i64,
}

/// Одно сообщение в диалоге (роль + текст, опц. прикреплённый документ, опц.
/// источники из базы). Приходит с фронтенда. `doc`/`sources` сериализуются только
/// когда есть (skip None), иначе пустые поля улетали бы в запрос к Ollama.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ChatMessage {
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
/// Status/Sources — только для онлайн-слоя (агентный цикл): статус «Ищу в интернете…»
/// и источники из веб-поиска. Офлайн-чат шлёт лишь Chunk/Thinking/Done — без изменений.
#[derive(Clone, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub(crate) enum ChatEvent {
    Chunk { content: String },
    Thinking { content: String },
    Status { content: String },
    Sources { items: Vec<tools::WebSource> },
    Notice { content: String },
    Done,
}

/// Стриминг ответа от Ollama. Фронт передаёт модель, историю и «канал»
/// `on_event`, в который мы по мере поступления шлём кусочки текста.
/// Весь сетевой трафик — строго на 127.0.0.1:11434 (правило проекта).
#[tauri::command]
pub(crate) async fn chat_stream(
    model: String,
    messages: Vec<ChatMessage>,
    think: bool,
    num_ctx: Option<u64>,
    gentle: Option<bool>,
    on_event: Channel<ChatEvent>,
    cancel: tauri::State<'_, CancelFlag>,
) -> Result<String, String> {
    let my_cancel = cancel.begin(); // свой флаг отмены на этот запрос (без гонок)
    // Контекст — рычаг смягчения (S2): меньше num_ctx → меньше KV-кэш → меньше памяти.
    let ctx = num_ctx.unwrap_or(8192);
    let mut body = serde_json::json!({
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
    apply_gentle(&mut body, gentle);
    // Стриминг вынесен в общий помощник: его же переиспользует агентный цикл
    // (онлайн-режим) для финального ответа — поведение офлайн-чата неизменно.
    stream_chat_response(body, &on_event, &my_cancel, ctx).await
}

/// Стриминг ответа Ollama по NDJSON в окно через Channel + watchdog по свопу (S3).
/// Возвращает полный текст ответа. Используется и обычным чатом (chat_stream), и
/// финальной репликой агентного цикла (онлайн-режим) — единый механизм стрима/отмены.
/// `cancel` уже взведён вызывающим при «Стоп»; здесь его только читаем.
pub(crate) async fn stream_chat_response(
    body: serde_json::Value,
    on_event: &Channel<ChatEvent>,
    cancel: &Arc<AtomicBool>,
    num_ctx: u64,
) -> Result<String, String> {
    // «Стоп» должен действовать уже НА ПОДКЛЮЧЕНИИ: холодная загрузка модели может
    // длиться десятки секунд, заголовков ещё нет — ждём send() с опросом отмены.
    let send_fut = HTTP.post("http://127.0.0.1:11434/api/chat").json(&body).send();
    let mut resp = match with_cancel(send_fut, cancel).await {
        Some(r) => r.map_err(|e| format!("Не удалось подключиться к Ollama: {e}"))?,
        None => return Ok(String::new()), // отменено до ответа — как обычная отмена стрима
    };

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(ollama_chat_error(status, &text));
    }

    // Watchdog (S3): лёгкая задача следит во время генерации за РОСТОМ свопа (своп —
    // триггер зависания) и за ПЕРЕГРЕВОМ (устойчиво выше TEMP_TRIP_C — риск аварийного
    // выключения слабой машины). При опасном пороге взводит тот же CancelFlag (как
    // «Стоп»), чтобы прервать запрос ДО неотзывчивости/ребута, и записывает причину.
    let wd_flag = cancel.clone();
    let wd_reason: Arc<std::sync::Mutex<Option<String>>> = Arc::new(std::sync::Mutex::new(None));
    let wd_reason_task = wd_reason.clone();
    let watchdog = tauri::async_runtime::spawn(async move {
        let mut sys = sysinfo::System::new();
        sys.refresh_memory();
        let base_swap = sys.used_swap();
        // Датчики температуры: Linux — hwmon, macOS — частично; на Windows sysinfo
        // отдаёт пустой список, и температурная ветка сама по себе бездействует.
        let mut components = sysinfo::Components::new_with_refreshed_list();
        let mut hot_ticks: u32 = 0; // подряд замеров с перегревом (выдержка против пиков)
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(WATCHDOG_INTERVAL_MS)).await;
            if wd_flag.load(Ordering::Relaxed) {
                break; // уже отменено («Стоп» или генерация завершена)
            }
            sys.refresh_memory();
            let growth = sys.used_swap().saturating_sub(base_swap);
            if growth > SWAP_TRIP_BYTES {
                *wd_reason_task.lock().unwrap_or_else(|e| e.into_inner()) = Some(
                    "недостаточно оперативной памяти — попробуйте модель полегче или \
                     меньший контекст"
                        .to_string(),
                );
                wd_flag.store(true, Ordering::Relaxed); // прерываем тем же механизмом
                break;
            }
            // Перегрев: берём максимум по всем датчикам (CPU или GPU — неважно, что
            // именно горячее). Один пик не считается — ждём TEMP_TRIP_TICKS подряд.
            components.refresh(false);
            let hottest = components
                .iter()
                .filter_map(|c| c.temperature())
                .fold(None::<f32>, |m, t| Some(m.map_or(t, |m| m.max(t))));
            if let Some(t) = hottest {
                if t >= TEMP_TRIP_C {
                    hot_ticks += 1;
                    if hot_ticks >= TEMP_TRIP_TICKS {
                        *wd_reason_task.lock().unwrap_or_else(|e| e.into_inner()) = Some(format!(
                            "компьютер перегревается ({t:.0}°C) — генерация остановлена, \
                             чтобы машина не выключилась. Дайте компьютеру остыть; \
                             «щадящий режим» в настройках снижает нагрев"
                        ));
                        wd_flag.store(true, Ordering::Relaxed);
                        break;
                    }
                } else {
                    hot_ticks = 0; // остыло — счётчик заново
                }
            }
        }
    });

    // Ollama отдаёт поток NDJSON — по одному JSON-объекту на строку.
    // full — полный текст ответа (авторитетный результат, без гонок на фронте).
    let mut full = String::new();

    // Разбор одной NDJSON-строки: thinking / content / done / error. Ошибку,
    // присланную Ollama посреди потока (упал runner, нехватка памяти), кладём в
    // stream_err — иначе ответ молча обрывается и пользователь не поймёт причину.
    let handle = |line: &str, full: &mut String, stream_err: &mut Option<String>| {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(line) {
            if let Some(e) = json.get("error").and_then(|e| e.as_str()) {
                *stream_err = Some(e.to_string());
                return;
            }
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
                // Признаки переполнения контекста: Ollama обрезала ответ по длине
                // (done_reason=="length") или промпт занял почти весь num_ctx — тогда
                // старая история/документ молча отброшены сервером. Честно сообщаем.
                let reason = json.get("done_reason").and_then(|r| r.as_str()).unwrap_or("");
                let peval = json
                    .get("prompt_eval_count")
                    .and_then(|c| c.as_u64())
                    .unwrap_or(0);
                if reason == "length" || (num_ctx > 0 && peval >= num_ctx * 95 / 100) {
                    let _ = on_event.send(ChatEvent::Notice {
                        content: "Контекст переполнен — модель могла не увидеть начало \
                                  диалога или часть документа. Начните новый диалог или \
                                  сократите приложенный текст."
                            .to_string(),
                    });
                }
            }
        }
    };

    // Остановить watchdog при ЛЮБОМ выходе из функции (успех, ошибка, отмена) —
    // иначе задача продолжит крутиться и может позже взвести чужой флаг отмены.
    let stop_watchdog = || watchdog.abort();

    // Ollama шлёт NDJSON. Копим БАЙТЫ (чтобы не резать UTF-8 на границе чанка) и
    // разбираем только целые строки; остаток без \n дочитываем после конца потока.
    let mut buf: Vec<u8> = Vec::new();
    let mut stream_err: Option<String> = None;
    loop {
        // «Стоп»/watchdog должны срабатывать, ДАЖЕ пока Ollama молчит (холодная
        // загрузка, залипание в свопе). Поэтому не блокируемся на chunk() навсегда:
        // ждём данные короткими окнами и между ними проверяем флаг отмены.
        if cancel.load(Ordering::Relaxed) {
            break; // «Стоп» или watchdog — рвём чтение, соединение закроется, Ollama остановит генерацию
        }
        let chunk = match tokio::time::timeout(
            std::time::Duration::from_millis(400),
            resp.chunk(),
        )
        .await
        {
            Err(_elapsed) => continue,   // за окно данных не пришло — перепроверяем отмену
            Ok(Ok(Some(chunk))) => chunk, // получили данные
            Ok(Ok(None)) => break,       // поток корректно завершился
            Ok(Err(e)) => {
                stop_watchdog();
                return Err(e.to_string());
            }
        };
        buf.extend_from_slice(&chunk);
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line_bytes: Vec<u8> = buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line_bytes);
            let line = line.trim();
            if !line.is_empty() {
                handle(line, &mut full, &mut stream_err);
            }
        }
        if stream_err.is_some() {
            break;
        }
    }
    // Хвост без завершающего перевода строки — здесь финальные токены ответа.
    if stream_err.is_none() && !buf.is_empty() {
        let line = String::from_utf8_lossy(&buf);
        let line = line.trim();
        if !line.is_empty() {
            handle(line, &mut full, &mut stream_err);
        }
    }

    stop_watchdog(); // генерация закончилась — watchdog больше не нужен
    // Ошибка, присланная самим Ollama в потоке — сообщаем пользователю явно.
    if let Some(e) = stream_err {
        return Err(format!("Ollama прервал генерацию: {e}"));
    }
    // Если прервал watchdog (а не пользователь) — отдаём точную причину как ошибку
    // (совет, что делать, входит в текст самой причины).
    if let Some(reason) = wd_reason.lock().unwrap_or_else(|e| e.into_inner()).take() {
        return Err(format!("Запрос остановлен: {reason}."));
    }
    Ok(full)
}

/// «Стоп»: помечаем текущий стрим на отмену. chat_stream увидит флаг и прервёт
/// чтение — соединение с Ollama закроется, генерация остановится (не жжём GPU).
/// Тот же флаг прерывает и агентный цикл (онлайн-режим): между раундами, перед
/// вызовом инструмента и в финальном стриме.
#[tauri::command]
pub(crate) fn cancel_stream(cancel: tauri::State<'_, CancelFlag>) {
    cancel
        .0
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .store(true, Ordering::Relaxed);
}

/// Человеческое объяснение ошибки /api/chat. Ollama отвечает JSON {"error": "..."},
/// причём текст бывает ЕЩЁ одним JSON от нового движка ({"error":{"message":…}}) —
/// пользователь видел сырое месиво из кавычек. Разворачиваем до двух слоёв и
/// переводим известные случаи на понятный язык.
pub(crate) fn ollama_chat_error(status: reqwest::StatusCode, body: &str) -> String {
    let mut msg = body.trim().to_string();
    for _ in 0..2 {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&msg) else { break };
        let Some(err) = v.get("error") else { break };
        msg = match err {
            serde_json::Value::String(s) => s.clone(),
            other => other
                .get("message")
                .and_then(|m| m.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| other.to_string()),
        };
    }
    if msg.contains("Failed to load image or audio file") {
        return "Модель не смогла прочитать приложенное изображение. Попробуйте сохранить \
                картинку в PNG или JPG и прикрепить снова."
            .into();
    }
    if msg.contains("does not support images") {
        return "Выбранная модель не работает с изображениями — установите модель зрения \
                (например, qwen3-vl) в настройках."
            .into();
    }
    if msg.is_empty() {
        return format!("Ollama вернул ошибку {status}");
    }
    format!("Ollama вернул ошибку {status}: {msg}")
}

#[cfg(test)]
mod tests {
    use super::ollama_chat_error;

    // Вложенный JSON-эрзац от нового движка разворачивается в человеческий текст.
    #[test]
    fn ollama_chat_error_unwraps_nested_json() {
        let body = r#"{"error":"{\"error\":{\"code\":400,\"message\":\"Failed to load image or audio file\",\"type\":\"invalid_request_error\"}}"}"#;
        let msg = ollama_chat_error(reqwest::StatusCode::BAD_REQUEST, body);
        assert!(msg.contains("изображение"), "{msg}");
        assert!(!msg.contains("Failed"), "сырой текст не должен доходить до пользователя: {msg}");

        let msg =
            ollama_chat_error(reqwest::StatusCode::NOT_FOUND, r#"{"error":"model not found"}"#);
        assert_eq!(msg, "Ollama вернул ошибку 404 Not Found: model not found");

        // Не-JSON тело показываем как есть (со статусом).
        let msg = ollama_chat_error(reqwest::StatusCode::INTERNAL_SERVER_ERROR, "boom");
        assert!(msg.contains("boom"));
    }
}
