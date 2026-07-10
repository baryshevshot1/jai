// Онлайн-слой (агентный режим): исходящий интернет-клиент, описания инструментов и
// их исполнители. Это ЕДИНСТВЕННОЕ место, откуда приложение ходит в интернет с
// данными пользователя — и только при явно включённом онлайн-режиме (152-ФЗ).
//
// Изоляция (правило проекта): клиент к Ollama — это `reqwest::Client::new()` на
// `http://127.0.0.1:11434` (localhost, без TLS-рукопожатия). Здесь же — ОТДЕЛЬНЫЙ
// клиент `web_client()` для HTTPS наружу (rustls), с таймаутами и User-Agent.
// Эти два пути не смешиваются: localhost остаётся как был, интернет — явный путь.
//
// Расширяемость: добавить новый инструмент = +1 запись в `tool_specs()` и +1 ветка
// в `execute_tool()`. Агентный цикл (lib.rs) дёргает инструменты обобщённо по имени
// и не переписывается при добавлении нового.

use serde::Serialize;
use std::time::Duration;

/// User-Agent исходящих запросов: честно представляемся провайдеру (некоторые API
/// отклоняют пустой UA). Версия берётся из Cargo, чтобы не расходилась с пакетом.
const USER_AGENT: &str = concat!("jai/", env!("CARGO_PKG_VERSION"), " (+offline-first assistant)");

/// Таймаут всего запроса наружу: интернет может «висеть», а пользователь ждёт ответа
/// модели — лучше быстро отдать честную ошибку, чем подвесить агентный цикл.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// Отдельный таймаут на установку соединения (нет сети/DNS не резолвится).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);

/// Сколько источников просим у провайдера (максимум Tavily). Широкий охват «разных
/// источников» за один поиск; у advanced-глубины число результатов НЕ влияет на расход
/// кредитов (те же 2 кредита), поэтому ширина бесплатна. Длину сниппетов ограничиваем
/// отдельно (ниже), чтобы 20 источников уместились в контекст модели «за раз».
const MAX_RESULTS: u64 = 20;
/// Глубина поиска Tavily: "advanced" — более полный и разнообразный охват источников,
/// чем "basic" (стоит дороже по кредитам, но даёт заметно лучшую широту).
const SEARCH_DEPTH: &str = "advanced";
/// Предел длины сниппета одного результата (символы). Заголовки и ссылки сохраняем
/// полностью — режем только тело, чтобы 20 источников уместились в num_ctx (≈ 20×330
/// симв ≈ 2200 токенов на поиск). Сводка (answer) идёт отдельно и не режется.
const SNIPPET_MAX_CHARS: usize = 200;

/// Исходящий HTTPS-клиент для инструментов (веб-поиск и далее). Отдельный экземпляр
/// от localhost-клиента Ollama. `https_only` запрещает случайный откат на http —
/// наружу ходим только по TLS (rustls, уже включён в Cargo.toml). Клиент дешёвый,
/// строим по требованию в исполнителе инструмента; пул соединений нам тут не нужен.
pub fn web_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(REQUEST_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        .https_only(true)
        .build()
        .map_err(|e| format!("Не удалось создать сетевой клиент: {e}"))
}

/// Источник из веб-поиска (заголовок + ссылка) — показываем пользователю в чате,
/// как RAG показывает источники из документов. Уходит на фронт через ChatEvent.
#[derive(Clone, Serialize)]
pub struct WebSource {
    pub title: String,
    pub url: String,
}

/// Результат исполнения инструмента: `content` уходит модели сообщением роли `tool`
/// (модель опирается на него в ответе), `sources` — пользователю в UI. На ошибках
/// (нет сети/ключа/результата) НЕ паникуем: возвращаем честный `content`, который
/// модель спокойно перескажет, и пустой `sources`.
/// `went_online` — было ли реальное обращение к провайдеру (данные могли уйти наружу).
/// По нему пишется журнал отправок: короткое замыкание без сети (нет ключа/URL/запроса)
/// в журнал НЕ попадает — он отражает только фактический выход в интернет (152-ФЗ).
pub struct ToolResult {
    pub content: String,
    pub sources: Vec<WebSource>,
    pub went_online: bool,
}

/// Конфиг веб-поиска из настроек (settings.json). Бэкенд НЕ захардкожен: провайдер,
/// эндпоинт и ключ задаются пользователем. По умолчанию — Tavily (см. дефолты в lib.rs).
pub struct WebSearchConfig {
    pub provider: String,
    pub url: String,
    pub api_key: String,
}

/// Описания инструментов для Ollama (массив `tools` в /api/chat). Формат — функции
/// с JSON-схемой параметров; по нему модель решает, какой инструмент вызвать и с
/// какими аргументами. Сейчас один инструмент — веб-поиск.
pub fn tool_specs() -> Vec<serde_json::Value> {
    vec![serde_json::json!({
        "type": "function",
        "function": {
            "name": "web_search",
            "description": "Поиск актуальной информации в интернете. Используй, когда \
                нужны свежие или проверяемые данные: новости, факты, цены/курсы, \
                законы, документация — то, чего нет в твоих знаниях или что могло \
                устареть. Возвращает заголовки, сниппеты и ссылки источников. Можно \
                вызывать несколько раз с разными формулировками запроса, чтобы \
                охватить разные источники и аспекты темы перед выводом.",
            "parameters": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Поисковый запрос на естественном языке (на языке вопроса пользователя)."
                    }
                },
                "required": ["query"]
            }
        }
    })]
}

/// Исполнить инструмент по имени и аргументам (как их вернула модель в tool_calls).
/// Диспетчер: добавление инструмента = новая ветка здесь + запись в tool_specs().
/// Неизвестное имя → честный tool-ответ (модель его перескажет), без паники.
pub async fn execute_tool(
    name: &str,
    args: &serde_json::Value,
    cfg: &WebSearchConfig,
) -> ToolResult {
    match name {
        "web_search" => web_search(args, cfg).await,
        other => ToolResult {
            content: format!("Инструмент «{other}» не поддерживается этим приложением."),
            sources: Vec::new(),
            went_online: false,
        },
    }
}

/// Исполнитель веб-поиска. Берёт `query` из аргументов модели, идёт к настроенному
/// провайдеру через изолированный HTTPS-клиент, возвращает модели краткую сводку +
/// результаты и список источников для UI. Все края (нет ключа/сети/результата) —
/// честным текстом в `content`, без падений.
async fn web_search(args: &serde_json::Value, cfg: &WebSearchConfig) -> ToolResult {
    // Короткое замыкание ДО сети (нет запроса/URL/ключа): наружу ничего не уходит,
    // в журнал не пишется → went_online: false.
    let offline_fail =
        |msg: String| ToolResult { content: msg, sources: Vec::new(), went_online: false };

    // 1) запрос из аргументов модели
    let query = args
        .get("query")
        .and_then(|q| q.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if query.is_empty() {
        return offline_fail("Веб-поиск не выполнен: пустой поисковый запрос.".into());
    }

    // 2) провайдер настроен? (Tavily требует ключ; без него — честно недоступен)
    if cfg.url.trim().is_empty() {
        return offline_fail(
            "Веб-поиск не настроен: не указан адрес поискового провайдера в настройках \
             (Онлайн-режим). Сообщи пользователю, что нужно настроить веб-поиск."
                .into(),
        );
    }
    let provider = if cfg.provider.trim().is_empty() { "tavily" } else { cfg.provider.trim() };
    if provider == "tavily" && cfg.api_key.trim().is_empty() {
        return offline_fail(
            "Веб-поиск недоступен: не указан API-ключ Tavily в настройках (Онлайн-режим). \
             Сообщи пользователю, что для веб-поиска нужно добавить ключ."
                .into(),
        );
    }

    // 3) клиент (ошибка сборки — тоже до фактического выхода в сеть)
    let client = match web_client() {
        Ok(c) => c,
        Err(e) => return offline_fail(format!("Веб-поиск недоступен: {e}")),
    };

    // 4) запрос к провайдеру (адаптер по имени провайдера — расширяемо). С этого
    // момента обращение наружу состоялось (или хотя бы началось) → went_online: true.
    let outcome = match provider {
        "tavily" => tavily_search(&client, cfg, &query).await,
        // Совместимый адаптер для прочих провайдеров с похожим JSON ({results:[{title,url,content}]})
        _ => generic_search(&client, cfg, &query).await,
    };

    match outcome {
        Ok(mut res) => {
            res.went_online = true;
            if res.sources.is_empty() && res.content.trim().is_empty() {
                res.content = format!("По запросу «{query}» ничего не найдено.");
            }
            res
        }
        Err(e) => ToolResult {
            content: format!(
                "Веб-поиск по запросу «{query}» не удался: {e}. Возможно, нет интернета или \
                 провайдер недоступен — сообщи об этом пользователю."
            ),
            sources: Vec::new(),
            went_online: true, // обращение к провайдеру было — могло уйти наружу
        },
    }
}

/// Адаптер Tavily (https://api.tavily.com/search). Авторизация — заголовок Bearer.
/// Ответ: { answer?, results: [{title, url, content, ...}] }.
async fn tavily_search(
    client: &reqwest::Client,
    cfg: &WebSearchConfig,
    query: &str,
) -> Result<ToolResult, String> {
    let body = serde_json::json!({
        "query": query,
        "search_depth": SEARCH_DEPTH,
        "max_results": MAX_RESULTS,
        "include_answer": "advanced",
        "topic": "general",
    });
    let resp = client
        .post(cfg.url.trim())
        .bearer_auth(cfg.api_key.trim())
        .json(&body)
        .send()
        .await
        .map_err(|e| friendly_net_err(&e))?;
    if !resp.status().is_success() {
        let status = resp.status();
        // 401/403 — почти всегда неверный/просроченный ключ.
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err("ключ Tavily отклонён (проверьте API-ключ в настройках)".into());
        }
        return Err(format!("провайдер вернул ошибку {status}"));
    }
    let json: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    let answer = json.get("answer").and_then(|a| a.as_str()).unwrap_or("");
    let results = json.get("results").and_then(|r| r.as_array());
    Ok(format_results(query, answer, results))
}

/// Обобщённый адаптер для провайдеров с JSON формата {results:[{title,url,content}]}
/// (и опц. поле answer). Ключ, если задан, шлём как Bearer. Позволяет подключить
/// другой бэкенд через настройки, не трогая код.
async fn generic_search(
    client: &reqwest::Client,
    cfg: &WebSearchConfig,
    query: &str,
) -> Result<ToolResult, String> {
    let body = serde_json::json!({ "query": query, "max_results": MAX_RESULTS });
    let mut req = client.post(cfg.url.trim()).json(&body);
    if !cfg.api_key.trim().is_empty() {
        req = req.bearer_auth(cfg.api_key.trim());
    }
    let resp = req.send().await.map_err(|e| friendly_net_err(&e))?;
    if !resp.status().is_success() {
        return Err(format!("провайдер вернул ошибку {}", resp.status()));
    }
    let json: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    let answer = json.get("answer").and_then(|a| a.as_str()).unwrap_or("");
    let results = json.get("results").and_then(|r| r.as_array());
    Ok(format_results(query, answer, results))
}

/// Собрать tool-сообщение (для модели) и список источников (для UI) из результатов
/// формата {title, url, content}. Сводку (answer) ставим первой — модель опирается
/// на неё, а пронумерованные источники помогают сослаться.
fn format_results(
    query: &str,
    answer: &str,
    results: Option<&Vec<serde_json::Value>>,
) -> ToolResult {
    let mut sources = Vec::new();
    let mut lines = Vec::new();
    if !answer.trim().is_empty() {
        lines.push(format!("Сводка: {}", answer.trim()));
    }
    if let Some(arr) = results {
        for (i, r) in arr.iter().enumerate() {
            let title = r.get("title").and_then(|t| t.as_str()).unwrap_or("").trim().to_string();
            let url = r.get("url").and_then(|u| u.as_str()).unwrap_or("").trim().to_string();
            let content_full = r.get("content").and_then(|c| c.as_str()).unwrap_or("").trim();
            // Режем тело сниппета по границе символов (не байтов — UTF-8), чтобы много
            // источников не переполнили контекст; заголовок и ссылку оставляем целиком.
            let content: String = if content_full.chars().count() > SNIPPET_MAX_CHARS {
                content_full.chars().take(SNIPPET_MAX_CHARS).collect::<String>() + "…"
            } else {
                content_full.to_string()
            };
            if url.is_empty() && title.is_empty() {
                continue;
            }
            let n = i + 1;
            lines.push(format!("[{n}] {title}\n{url}\n{content}"));
            sources.push(WebSource {
                title: if title.is_empty() { url.clone() } else { title },
                url,
            });
        }
    }
    let content = if lines.is_empty() {
        format!("По запросу «{query}» ничего не найдено.")
    } else {
        format!("Результаты веб-поиска по запросу «{query}»:\n\n{}", lines.join("\n\n"))
    };
    // Вызывается только после ответа провайдера → обращение наружу состоялось.
    ToolResult { content, sources, went_online: true }
}

/// Превратить сетевую ошибку reqwest в человеческое объяснение (без интернета /
/// таймаут / прочее) — модель перескажет это пользователю спокойно.
fn friendly_net_err(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "превышено время ожидания ответа".into()
    } else if e.is_connect() {
        "не удалось соединиться (вероятно, нет интернета)".into()
    } else {
        e.to_string()
    }
}
