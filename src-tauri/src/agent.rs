// ── Онлайн-слой (агентный режим): цикл tool calling + журнал исходящих ───────
// Единственное место, откуда наружу уходят данные пользователя, и то лишь при явно
// включённом тумблере. Инструменты живут в tools.rs, стрим финального ответа — общий
// с офлайн-чатом (chat.rs).

use serde::{Deserialize, Serialize};
use std::sync::atomic::Ordering;
use tauri::ipc::Channel;
use tauri::Manager;

use crate::chat::{apply_gentle, ollama_chat_error, stream_chat_response, ChatEvent};
use crate::error::AppResult;
use crate::settings::read_settings;
use crate::{now_ms, tools, with_cancel, write_atomic, CancelFlag, HTTP};

// ── Цикл tool calling вокруг чата ────────────────────────────────────────────
// Включается ТОЛЬКО при явном онлайн-режиме и модели с возможностью `tools`. Офлайн-
// чат сюда не заходит (фронт идёт прежним путём chat_stream). 152-ФЗ: наружу данные
// уходят лишь здесь, видимо (индикатор) и под запись (лог исходящих обращений).

/// Предел раундов инструментов — защита от бесконечного цикла агента. При превышении
/// корректно завершаем финальным ответом по уже собранным данным.
const MAX_TOOL_ROUNDS: usize = 4;

/// Предел числа РЕАЛЬНЫХ веб-поисков на один вопрос. Каждый возвращает до 20 источников
/// (≈2200 токенов), поэтому объём суммарно ограничиваем, чтобы всё уместилось в num_ctx
/// и обрабатывалось «за раз». После лимита инструменты не передаём — модель обязана
/// дать ответ с анализом по уже собранному (до 2×20 = 40 источников).
const MAX_SEARCHES: usize = 2;

/// Предел числа прочитанных страниц (read_url) на один вопрос. Каждая — до ~4000
/// символов (≈2000 токенов, см. PAGE_TEXT_MAX_CHARS в tools.rs); две страницы плюс
/// поиски — потолок разумного объёма для num_ctx 8k. Бюджет отдельный от поисков:
/// «дал ссылку — прочитай» не должен тратить лимит поисков и наоборот.
const MAX_READS: usize = 2;

/// Дефолты веб-поиска (бэкенд конфигурируемый — это лишь значения по умолчанию).
const DEFAULT_SEARCH_PROVIDER: &str = "tavily";
const DEFAULT_SEARCH_URL: &str = "https://api.tavily.com/search";

/// Прочитать конфиг веб-поиска из settings.json (провайдер/эндпоинт/ключ). Пустые
/// ключи → дефолты Tavily; API-ключ по умолчанию пуст (без него инструмент честно
/// недоступен). Чтение строго локальное.
fn read_web_search_config(app: &tauri::AppHandle) -> tools::WebSearchConfig {
    let map = read_settings(app);
    let get = |k: &str| map.get(k).and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let provider = {
        let p = get("web_search_provider");
        if p.is_empty() { DEFAULT_SEARCH_PROVIDER.to_string() } else { p }
    };
    let url = {
        let u = get("web_search_url");
        if u.is_empty() { DEFAULT_SEARCH_URL.to_string() } else { u }
    };
    tools::WebSearchConfig {
        provider,
        url,
        api_key: get("web_search_api_key"),
    }
}

/// Агентный цикл tool calling вокруг чата. Делает «решающие» ходы к Ollama без
/// стриминга (с массивом `tools`), исполняет инструменты в Rust, а ФИНАЛЬНЫЙ ответ
/// стримит тем же механизмом, что и обычный чат (stream_chat_response). Уважает
/// CancelFlag на всех стадиях. `messages` — сырые {role, content, images?} с фронта;
/// по ходу дополняются ассистентскими tool_calls и результатами роли `tool`.
// Аргументы — это контракт IPC с фронтендом (те же поля, что у chat_stream);
// собирать их в структуру ради счётчика clippy значит усложнить вызов из JS.
#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub(crate) async fn agentic_chat(
    app: tauri::AppHandle,
    model: String,
    messages: Vec<serde_json::Value>,
    think: bool,
    num_ctx: Option<u64>,
    gentle: Option<bool>,
    on_event: Channel<ChatEvent>,
    cancel: tauri::State<'_, CancelFlag>,
) -> AppResult<String> {
    // Серверный гейт онлайн-режима (152-ФЗ): данные уходят наружу только при явно
    // включённом пользователем тумблере. Фронтенд проверяет то же самое, но
    // авторитет — здесь: при выключенной настройке команда отказывает ДО единого
    // исходящего запроса, каким бы путём её ни вызвали.
    let online_on = read_settings(&app).get("online_mode").and_then(|v| v.as_str()) == Some("1");
    if !online_on {
        return Err("Онлайн-режим выключен — выход в интернет запрещён. Включите его \
                    тумблером или задайте вопрос в обычном (офлайн) чате."
            .into());
    }
    let my_cancel = cancel.begin(); // свой флаг отмены на этот запрос (без гонок)
    let ctx = num_ctx.unwrap_or(8192);
    let cfg = read_web_search_config(&app);
    let tool_specs = tools::tool_specs();

    let mut msgs = messages;
    let mut searches_done: usize = 0; // сколько веб-поисков уже выполнено
    let mut reads_done: usize = 0; // сколько страниц уже прочитано (read_url)

    // Раунды инструментов: пока модель просит вызовы — исполняем и продолжаем.
    for round in 0..MAX_TOOL_ROUNDS {
        if my_cancel.load(Ordering::Relaxed) {
            return Ok(String::new()); // «Стоп» — выходим тихо (фронт игнорирует хвост)
        }
        let _ = on_event.send(ChatEvent::Status {
            content: if round == 0 { "Думаю…".into() } else { "Обрабатываю найденное…".into() },
        });

        // Решающий ход: без стриминга, рассуждения выключены (быстрее). Инструменты
        // передаём, пока не исчерпан лимит поисков; после — без tools, чтобы модель
        // обработала собранное и дала ответ (защита контекста и от лишних обращений).
        let mut body = serde_json::json!({
            "model": model,
            "messages": msgs,
            "stream": false,
            "think": false,
            "options": { "num_ctx": ctx },
        });
        apply_gentle(&mut body, gentle);
        if searches_done < MAX_SEARCHES || reads_done < MAX_READS {
            body["tools"] = serde_json::json!(tool_specs);
        }
        // Решающий ход без стриминга может думать долго — ждём с опросом отмены,
        // чтобы «Стоп» действовал и ПОСРЕДИ этого запроса, а не только между раундами.
        let send_fut = HTTP.post("http://127.0.0.1:11434/api/chat").json(&body).send();
        let resp = match with_cancel(send_fut, &my_cancel).await {
            Some(r) => r.map_err(|e| format!("Не удалось подключиться к Ollama: {e}"))?,
            None => return Ok(String::new()), // «Стоп» посреди решающего хода
        };
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(ollama_chat_error(status, &text));
        }
        let json: serde_json::Value =
            match with_cancel(resp.json::<serde_json::Value>(), &my_cancel).await {
                Some(r) => r.map_err(|e| e.to_string())?,
                None => return Ok(String::new()),
            };
        let message = json.get("message").cloned().unwrap_or(serde_json::Value::Null);
        let tool_calls = message
            .get("tool_calls")
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default();

        if tool_calls.is_empty() {
            break; // модель готова отвечать → выходим на финальный стрим
        }

        // Добавляем ход ассистента (с tool_calls) дословно — Ollama ждёт его в истории.
        msgs.push(message.clone());

        // Исполняем каждый запрошенный инструмент в Rust.
        for tc in &tool_calls {
            if my_cancel.load(Ordering::Relaxed) {
                return Ok(String::new());
            }
            let fname = tc
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();
            let fargs = tc
                .get("function")
                .and_then(|f| f.get("arguments"))
                .cloned()
                .unwrap_or(serde_json::Value::Null);

            // Бюджет СВОЕГО инструмента исчерпан (в т. ч. если модель запросила
            // несколько вызовов в одном раунде) — лишние не исполняем, честно помечаем,
            // чтобы модель перешла к анализу и выводу по уже собранным данным.
            let exhausted = match fname.as_str() {
                "web_search" => searches_done >= MAX_SEARCHES,
                "read_url" => reads_done >= MAX_READS,
                _ => false, // неизвестный инструмент бюджета не имеет (execute_tool честно откажет)
            };
            if exhausted {
                msgs.push(serde_json::json!({
                    "role": "tool",
                    "tool_name": fname,
                    "content": "Достигнут предел обращений этого инструмента. Больше не вызывай \
                                его — проанализируй уже собранное и дай итоговый ответ.",
                }));
                continue;
            }

            // Статус — с деталью, что именно уходит наружу (прозрачность в интерфейсе).
            let _ = on_event.send(ChatEvent::Status {
                content: match fname.as_str() {
                    "read_url" => {
                        let host = fargs
                            .get("url")
                            .and_then(|u| u.as_str())
                            .and_then(|u| reqwest::Url::parse(u.trim()).ok())
                            .and_then(|u| u.host_str().map(str::to_string))
                            .unwrap_or_default();
                        if host.is_empty() {
                            "Читаю страницу…".into()
                        } else {
                            format!("Читаю страницу {host}…")
                        }
                    }
                    _ => {
                        let q = fargs.get("query").and_then(|q| q.as_str()).unwrap_or("").trim();
                        let short: String = q.chars().take(60).collect();
                        let ell = if q.chars().count() > 60 { "…" } else { "" };
                        if short.is_empty() {
                            "Ищу в интернете…".into()
                        } else {
                            format!("Ищу в интернете: «{short}{ell}»")
                        }
                    }
                },
            });

            // НЕ оборачиваем в with_cancel: журнал исходящих (152-ФЗ) пишется ПОСЛЕ
            // вызова — обрыв посередине оставил бы уход в сеть без записи в журнал.
            // Сам вызов ограничен таймаутами web_client (8 с подключение / 20 с всего).
            let result = tools::execute_tool(&fname, &fargs, &cfg).await;
            match fname.as_str() {
                "web_search" => searches_done += 1,
                "read_url" => reads_done += 1,
                _ => {}
            }

            // Журнал исходящих обращений (152-ФЗ: что и на какой хост ушло) — пишем
            // ТОЛЬКО при реальном выходе в сеть (went_online). Короткое замыкание без
            // ключа/сети наружу ничего не отправляет и в журнал не попадает.
            if result.went_online {
                let (host, query) = if fname == "read_url" {
                    // Для чтения страницы «хост» и «запрос» — адрес самой страницы:
                    // в журнале видно, куда именно ходили. Берём ФАКТИЧЕСКИЙ адрес
                    // после редиректов (final_url): иначе журнал 152-ФЗ показывал бы
                    // ссылку, которую попросили открыть, а не ту, что реально прочли.
                    let asked =
                        fargs.get("url").and_then(|q| q.as_str()).unwrap_or("").trim().to_string();
                    let reached = result.final_url.clone().unwrap_or_else(|| asked.clone());
                    let host = reqwest::Url::parse(&reached)
                        .ok()
                        .and_then(|p| p.host_str().map(str::to_string))
                        .unwrap_or_else(|| reached.clone());
                    // Если увели редиректом — в журнале видно обе точки цепочки.
                    let shown = if reached == asked || asked.is_empty() {
                        reached
                    } else {
                        format!("{asked} → {reached}")
                    };
                    (host, shown)
                } else {
                    let q =
                        fargs.get("query").and_then(|q| q.as_str()).unwrap_or("").to_string();
                    // Адрес провайдера тоже после редиректов — куда запрос дошёл на деле.
                    let host = result.final_url.clone().unwrap_or_else(|| cfg.url.clone());
                    (host, q)
                };
                log_outbound(&app, &host, &fname, &query);
            }
            if !result.sources.is_empty() {
                let _ = on_event.send(ChatEvent::Sources { items: result.sources.clone() });
            }
            // Результат — сообщением роли `tool` (модель на него опирается).
            msgs.push(serde_json::json!({
                "role": "tool",
                "tool_name": fname,
                "content": result.content,
            }));
        }
        // Если это был последний разрешённый раунд — дальше финал по собранному.
        if round + 1 == MAX_TOOL_ROUNDS {
            let _ = on_event.send(ChatEvent::Status {
                content: "Готовлю ответ по найденному…".into(),
            });
        }
    }

    if my_cancel.load(Ordering::Relaxed) {
        return Ok(String::new());
    }

    // Финальный ответ — СТРИМИМ существующим механизмом (без инструментов, чтобы
    // модель дала текст), с рассуждениями по тумблеру пользователя.
    let mut final_body = serde_json::json!({
        "model": model,
        "messages": msgs,
        "stream": true,
        "think": think,
        "options": { "num_ctx": ctx },
    });
    apply_gentle(&mut final_body, gentle);
    stream_chat_response(final_body, &on_event, &my_cancel, ctx).await
}

// ── Лог исходящих обращений (152-ФЗ): прозрачность постфактум ─────────────────

/// Запись лога: время, хост, инструмент, запрос. Хранится локально в appDataDir.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct OutboundLogEntry {
    ts: i64,
    host: String,
    tool: String,
    query: String,
}

/// Путь файла лога исходящих обращений (JSON-массив) в appDataDir.
fn outbound_log_path(app: &tauri::AppHandle) -> Result<std::path::PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Не удалось получить appDataDir: {e}"))?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("Не удалось создать каталог: {e}"))?;
    Ok(dir.join("outbound_log.json"))
}

/// Прочитать лог (новые в конце файла). Сбой чтения → пустой (лог не критичен).
fn read_outbound_log(app: &tauri::AppHandle) -> Vec<OutboundLogEntry> {
    if let Ok(path) = outbound_log_path(app) {
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(v) = serde_json::from_str::<Vec<OutboundLogEntry>>(&text) {
                return v;
            }
        }
    }
    Vec::new()
}

/// Дописать обращение в лог (хост из URL). Ограничиваем последними 200 записями,
/// чтобы файл не рос бесконечно. Ошибки записи гасим — лог не должен ломать чат.
fn log_outbound(app: &tauri::AppHandle, url: &str, tool: &str, query: &str) {
    let host = reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
        .unwrap_or_else(|| url.to_string());
    let mut log = read_outbound_log(app);
    log.push(OutboundLogEntry {
        ts: now_ms(),
        host,
        tool: tool.to_string(),
        query: query.chars().take(300).collect(), // не храним гигантские запросы
    });
    let len = log.len();
    if len > 200 {
        log.drain(0..len - 200);
    }
    if let Ok(path) = outbound_log_path(app) {
        if let Ok(text) = serde_json::to_string_pretty(&log) {
            let _ = write_atomic(&path, &text);
        }
    }
}

/// Лог исходящих обращений для интерфейса (новые сверху).
#[tauri::command]
pub(crate) fn get_outbound_log(app: tauri::AppHandle) -> Vec<OutboundLogEntry> {
    let mut log = read_outbound_log(&app);
    log.reverse();
    log
}

/// Очистить лог исходящих обращений.
#[tauri::command]
pub(crate) fn clear_outbound_log(app: tauri::AppHandle) -> Result<(), String> {
    let path = outbound_log_path(&app)?;
    if path.exists() {
        std::fs::remove_file(&path).map_err(|e| format!("Не удалось очистить лог: {e}"))?;
    }
    Ok(())
}
