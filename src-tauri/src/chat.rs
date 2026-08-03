// ── Чат: стриминг ответа Ollama, «щадящий режим», watchdog и «Стоп» ──────────
// Ядро офлайн-чата: сообщения и события IPC, стрим NDJSON от движка, сторож памяти
// и перегрева. Тот же стрим переиспользует агентный цикл (онлайн-слой, agent.rs).

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::ipc::Channel;

use crate::error::{AppError, AppResult, ErrorCode};
use crate::{tools, with_cancel, CancelFlag, HTTP};

/// Watchdog (S3): период замеров и порог роста свопа, при котором прерываем запрос.
/// Консервативно (≥512 МБ роста = модель реально сливается в своп), чтобы не рубить
/// валидные запросы из-за мелких колебаний. Лёгкий: замер раз в 1.5 с.
const WATCHDOG_INTERVAL_MS: u64 = 1500;
const SWAP_TRIP_BYTES: u64 = 512 * 1024 * 1024;

/// Порог свободной памяти, ниже которого генерацию прерываем. Своп — признак уже
/// НАЧАВШЕЙСЯ беды, и он есть не везде: на машине без свопа (или когда память
/// кончается быстрее, чем ОС успевает свопить) сторож по свопу молчит, а процесс
/// убивает системный OOM-killer — для пользователя это «приложение вылетело».
/// Поэтому следим и за самой доступной памятью: 512 МБ — это уже край, дальше ОС
/// начинает убивать процессы.
const LOW_MEMORY_BYTES: u64 = 512 * 1024 * 1024;

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

/// Сколько движок держит модель в памяти после ответа. Обычно 5 минут: следующий
/// вопрос идёт без перезагрузки весов (это десятки секунд). В щадящем режиме —
/// полминуты: он включается на слабых машинах, где освободить память важнее, чем
/// сэкономить время на повторной загрузке.
///
/// Передаём ИМЕННО В ЗАПРОСЕ, а не переменной окружения. Переменные действуют,
/// только когда движок поднимаем мы; при системной установке Ollama (а install.sh
/// её и рекомендует) приложение переиспользует чужой процесс, и весь экономный
/// профиль мимо. Значение в теле запроса движок уважает в любом случае.
const KEEP_ALIVE_DEFAULT: &str = "5m";
const KEEP_ALIVE_GENTLE: &str = "30s";

/// Применить «щадящий режим» к телу запроса /api/chat: ограничить потоки CPU и
/// быстрее освобождать память под моделью.
/// Пер-запросная опция — действует на любой движок (наш или системный).
pub(crate) fn apply_gentle(body: &mut serde_json::Value, gentle: Option<bool>) {
    body["keep_alive"] = serde_json::json!(if gentle.unwrap_or(false) {
        KEEP_ALIVE_GENTLE
    } else {
        KEEP_ALIVE_DEFAULT
    });
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
    // ── Прозрачность онлайн-режима (152-ФЗ) ──────────────────────────────
    // Эти два поля кладёт интерфейс при сохранении ответа, построенного с
    // выходом в интернет: список сайтов и саму метку «интернет использовался».
    //
    // Их здесь НЕ БЫЛО, а история сохраняется через эту же структуру — значит,
    // serde молча выбрасывал их при записи. Пользователь видел источники ровно до
    // закрытия окна: назавтра тот же ответ выглядел как обычный офлайновый, и
    // проверить, откуда взята норма и что уходило наружу, было уже нечем.
    // Для продукта, у которого прозрачность выхода в интернет — заявленное
    // обещание, это потеря не украшения, а доказательства.
    #[serde(rename = "webSources", default, skip_serializing_if = "Option::is_none")]
    web_sources: Option<Vec<crate::tools::WebSource>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    online: Option<bool>,
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
) -> AppResult<String> {
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
) -> AppResult<String> {
    // «Стоп» должен действовать уже НА ПОДКЛЮЧЕНИИ: холодная загрузка модели может
    // длиться десятки секунд, заголовков ещё нет — ждём send() с опросом отмены.
    let send_fut = HTTP.post("http://127.0.0.1:11434/api/chat").json(&body).send();
    let mut resp = match with_cancel(send_fut, cancel).await {
        Some(r) => r.map_err(|e| AppError::from_reqwest(&e, "Не удалось подключиться к движку"))?,
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
    // Причина остановки — вместе с кодом: сторож точно знает, память это или перегрев,
    // и интерфейсу незачем угадывать это по формулировке.
    let wd_reason: Arc<std::sync::Mutex<Option<(ErrorCode, String)>>> =
        Arc::new(std::sync::Mutex::new(None));
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
            // Две независимые приметы беды. Своп растёт — машина уже «легла» и
            // отвечает рывками. Свободная память на нуле — до OOM-killer секунды,
            // и свопа может не быть вовсе. Ловим то, что случится раньше.
            let growth = sys.used_swap().saturating_sub(base_swap);
            // available_bytes даёт None, когда ОС не отвечает осмысленно (на macOS
            // это штатно при разросшемся компрессоре — см. memory::available_bytes).
            // «Не знаю» — НЕ повод рубить генерацию: раньше именно так обрывался
            // каждый ответ на здоровой машине. Тогда защиту несёт рост свопа.
            let available = crate::memory::available_bytes(&sys);
            let low_mem = available.is_some_and(|a| a < LOW_MEMORY_BYTES);
            if growth > SWAP_TRIP_BYTES || low_mem {
                let mb = |b: u64| b / 1024 / 1024;
                crate::journal::warn(
                    "чат",
                    format!(
                        "генерация прервана сторожем: рост свопа {} МБ (порог {}), \
                         доступно {} (порог {} МБ)",
                        mb(growth),
                        mb(SWAP_TRIP_BYTES),
                        available.map(|a| format!("{} МБ", mb(a))).unwrap_or("неизвестно".into()),
                        mb(LOW_MEMORY_BYTES),
                    ),
                );
                *wd_reason_task.lock().unwrap_or_else(|e| e.into_inner()) = Some((
                    ErrorCode::OutOfMemory,
                    "недостаточно оперативной памяти — попробуйте модель полегче или \
                     меньший контекст"
                        .to_string(),
                ));
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
                        *wd_reason_task.lock().unwrap_or_else(|e| e.into_inner()) = Some((
                            ErrorCode::Unknown,
                            format!(
                                "компьютер перегревается ({t:.0}°C) — генерация остановлена, \
                                 чтобы машина не выключилась. Дайте компьютеру остыть; \
                                 «щадящий режим» в настройках снижает нагрев"
                            ),
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
                return Err(AppError::from_reqwest(&e, "Ответ движка оборвался"));
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
        // Движок прислал ошибку прямо в потоке — причину называем той же логикой,
        // что и для ответа с HTTP-ошибкой: место одно, поведение одинаковое.
        return Err(AppError::new(
            classify_ollama_message(&e),
            format!("Движок прервал генерацию: {e}"),
        ));
    }
    // Если прервал watchdog (а не пользователь) — отдаём точную причину как ошибку
    // (совет, что делать, входит в текст самой причины).
    if let Some((code, reason)) = wd_reason.lock().unwrap_or_else(|e| e.into_inner()).take() {
        return Err(AppError::new(code, format!("Запрос остановлен: {reason}.")));
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
/// Причина сбоя по тексту сообщения ДВИЖКА. Разбор текста тут неизбежен — движок
/// не присылает машинных кодов, — но он живёт в одном месте и применяется только к
/// сообщениям Ollama, а не к произвольной строке, как было на фронтенде.
fn classify_ollama_message(msg: &str) -> ErrorCode {
    let lower = msg.to_ascii_lowercase();
    if lower.contains("try pulling") || (lower.contains("model") && lower.contains("not found")) {
        return ErrorCode::ModelMissing;
    }
    if lower.contains("out of memory")
        || lower.contains("not enough memory")
        || lower.contains("requires more")
        || lower.contains("failed to allocate")
        || lower.contains("cudamalloc")
    {
        return ErrorCode::OutOfMemory;
    }
    ErrorCode::Unknown
}

pub(crate) fn ollama_chat_error(status: reqwest::StatusCode, body: &str) -> AppError {
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
    // Причину называем ЗДЕСЬ, где виден и статус, и распакованное тело: интерфейсу
    // достаётся код, а не текст на разбор регулярками.
    match classify_ollama_message(&msg) {
        ErrorCode::ModelMissing => {
            return AppError::new(
                ErrorCode::ModelMissing,
                format!("Модель недоступна в движке: {msg}"),
            )
        }
        ErrorCode::OutOfMemory => {
            return AppError::new(
                ErrorCode::OutOfMemory,
                format!("Не хватило памяти под модель: {msg}"),
            )
        }
        _ if status == reqwest::StatusCode::NOT_FOUND => {
            return AppError::new(
                ErrorCode::ModelMissing,
                format!("Модель недоступна в движке: {msg}"),
            )
        }
        _ => {}
    }
    if msg.contains("Failed to load image or audio file") {
        return AppError::unknown(
            "Модель не смогла прочитать приложенное изображение. Попробуйте сохранить \
             картинку в PNG или JPG и прикрепить снова.",
        );
    }
    if msg.contains("does not support images") {
        return AppError::unknown(
            "Выбранная модель не работает с изображениями — установите модель зрения \
             (например, qwen3-vl) в настройках.",
        );
    }
    if msg.is_empty() {
        return AppError::unknown(format!("Ollama вернул ошибку {status}"));
    }
    AppError::unknown(format!("Ollama вернул ошибку {status}: {msg}"))
}

#[cfg(test)]
mod tests {
    use super::{classify_ollama_message, ollama_chat_error};
    use crate::error::ErrorCode;

    // Вложенный JSON-эрзац от нового движка разворачивается в человеческий текст.
    #[test]
    fn ollama_chat_error_unwraps_nested_json() {
        let body = r#"{"error":"{\"error\":{\"code\":400,\"message\":\"Failed to load image or audio file\",\"type\":\"invalid_request_error\"}}"}"#;
        let e = ollama_chat_error(reqwest::StatusCode::BAD_REQUEST, body);
        assert!(e.message.contains("изображение"), "{}", e.message);
        assert!(
            !e.message.contains("Failed"),
            "сырой текст не должен доходить до пользователя: {}",
            e.message
        );

        // Не-JSON тело показываем как есть (со статусом).
        let e = ollama_chat_error(reqwest::StatusCode::INTERNAL_SERVER_ERROR, "boom");
        assert!(e.message.contains("boom"));
        assert_eq!(e.code, ErrorCode::Unknown);
    }

    // Причину сбоя называет бэкенд: интерфейс получает код и не разбирает текст.
    #[test]
    fn ollama_chat_error_reports_reason_code() {
        let e = ollama_chat_error(reqwest::StatusCode::NOT_FOUND, r#"{"error":"model not found"}"#);
        assert_eq!(e.code, ErrorCode::ModelMissing);

        // 404 без внятного тела — всё равно «модели нет»: именно этим статусом
        // Ollama отвечает на запрос к неустановленной модели.
        let e = ollama_chat_error(reqwest::StatusCode::NOT_FOUND, "");
        assert_eq!(e.code, ErrorCode::ModelMissing);

        let e = ollama_chat_error(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            r#"{"error":"model requires more system memory (9.2 GiB) than is available"}"#,
        );
        assert_eq!(e.code, ErrorCode::OutOfMemory);
    }

    #[test]
    fn classify_knows_missing_model_and_memory() {
        assert_eq!(
            classify_ollama_message("model \"qwen3.5:9b\" not found, try pulling it first"),
            ErrorCode::ModelMissing
        );
        assert_eq!(classify_ollama_message("CUDA error: cudaMalloc failed"), ErrorCode::OutOfMemory);
        assert_eq!(classify_ollama_message("something odd happened"), ErrorCode::Unknown);
        // «runtime» не должен сойти за таймаут, а «no such file» — за отсутствие модели:
        // ровно на этих ложных срабатываниях ломался прежний разбор на фронтенде.
        assert_eq!(classify_ollama_message("panic in runtime"), ErrorCode::Unknown);
        assert_eq!(classify_ollama_message("no such file or directory"), ErrorCode::Unknown);
    }
}

#[cfg(test)]
mod gentle_tests {
    use super::apply_gentle;

    /// keep_alive обязан уходить В ТЕЛЕ запроса. Переменные окружения действуют,
    /// только когда движок поднимает само приложение; при системной установке Ollama
    /// (её и рекомендует install.sh) весь экономный профиль проходит мимо, и
    /// keep_alive в запросе остаётся единственным рычагом, который движок уважает.
    #[test]
    fn keep_alive_is_always_sent() {
        for gentle in [None, Some(false), Some(true)] {
            let mut body = serde_json::json!({ "model": "m" });
            apply_gentle(&mut body, gentle);
            assert!(
                body["keep_alive"].is_string(),
                "keep_alive пропал при gentle={gentle:?}: на чужом движке модель \
                 останется висеть в памяти"
            );
        }
    }

    /// В щадящем режиме (слабая машина) память освобождаем заметно быстрее:
    /// там важнее отдать её системе, чем сэкономить на повторной загрузке весов.
    #[test]
    fn gentle_mode_frees_memory_sooner() {
        let mut normal = serde_json::json!({});
        apply_gentle(&mut normal, Some(false));
        let mut gentle = serde_json::json!({});
        apply_gentle(&mut gentle, Some(true));

        assert_eq!(normal["keep_alive"], "5m");
        assert_eq!(gentle["keep_alive"], "30s");
        // И потоки процессора в щадящем режиме ограничены — это его прежний смысл.
        assert!(gentle["options"]["num_thread"].is_number());
        assert!(normal["options"]["num_thread"].is_null());
    }
}

#[cfg(test)]
mod online_history_tests {
    use super::*;

    /// Следы обращения в интернет обязаны пережить сохранение диалога.
    ///
    /// История пишется через эту самую структуру. Полей `webSources` и `online` в
    /// ней не было — serde молча выбрасывал их при записи, и пользователь видел
    /// источники ровно до закрытия окна. Назавтра тот же ответ выглядел как
    /// обычный офлайновый: проверить, откуда взята норма и что уходило наружу,
    /// было уже нечем. Для продукта, обещающего прозрачность выхода в интернет,
    /// это потеря доказательства, а не оформления.
    #[test]
    fn web_sources_survive_saving_a_conversation() {
        let saved = serde_json::json!({
            "role": "assistant",
            "content": "Согласно статье 5…",
            "webSources": [{ "title": "КонсультантПлюс", "url": "https://example.org/a" }],
            "online": true
        });

        let msg: ChatMessage = serde_json::from_value(saved).expect("история не разобралась");
        let back = serde_json::to_value(&msg).expect("история не сериализовалась");

        assert_eq!(back["online"], serde_json::json!(true), "метка «интернет» потеряна");
        let sources = back["webSources"].as_array().expect("список источников потерян");
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0]["url"], "https://example.org/a");
        assert_eq!(sources[0]["title"], "КонсультантПлюс");
    }

    /// А у обычного офлайнового ответа этих полей быть не должно — иначе они
    /// уезжали бы в запрос к Ollama пустыми на каждом сообщении.
    #[test]
    fn offline_answer_carries_no_online_fields() {
        let msg: ChatMessage =
            serde_json::from_value(serde_json::json!({ "role": "user", "content": "привет" }))
                .unwrap();
        let back = serde_json::to_value(&msg).unwrap();
        assert!(back.get("webSources").is_none(), "пустые источники не должны сериализоваться");
        assert!(back.get("online").is_none(), "пустая метка не должна сериализоваться");
    }
}
