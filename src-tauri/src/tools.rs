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

/// Максимальный размер тела страницы, который согласны скачать при чтении ссылки
/// (read_url): защита трафика и памяти. Больше — честно отказываемся.
const PAGE_MAX_BYTES: usize = 2 * 1024 * 1024;
/// Предел извлечённого текста страницы для модели (символы): ~2000 токенов по-русски.
/// Держим умеренным: до двух страниц за вопрос (MAX_READS в lib.rs) плюс результаты
/// поиска должны умещаться в num_ctx 8k вместе с историей.
const PAGE_TEXT_MAX_CHARS: usize = 4000;

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
/// какими аргументами. Инструменты: веб-поиск и чтение страницы по ссылке.
pub fn tool_specs() -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({
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
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "read_url",
                "description": "Открыть страницу по https-ссылке и прочитать её текст. \
                    Используй, когда пользователь дал конкретную ссылку, которую нужно \
                    изучить, или когда сниппетов веб-поиска мало и стоит прочитать самый \
                    релевантный источник целиком. Возвращает извлечённый текст страницы \
                    (без вёрстки).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "url": {
                            "type": "string",
                            "description": "Полная https-ссылка на страницу (например, из результатов web_search или из вопроса пользователя)."
                        }
                    },
                    "required": ["url"]
                }
            }
        }),
    ]
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
        "read_url" => read_url(args).await,
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

/// Исполнитель чтения страницы (read_url). Открывает https-ссылку изолированным
/// клиентом, извлекает читаемый текст (без вёрстки) и отдаёт модели. Все края —
/// честным текстом в `content`: модель перескажет причину пользователю.
async fn read_url(args: &serde_json::Value) -> ToolResult {
    // Отказы ДО сети (кривая ссылка, http, локальный адрес): наружу ничего не ушло.
    let offline_fail =
        |msg: String| ToolResult { content: msg, sources: Vec::new(), went_online: false };

    let raw = args.get("url").and_then(|u| u.as_str()).unwrap_or("").trim().to_string();
    if raw.is_empty() {
        return offline_fail("Чтение страницы не выполнено: пустая ссылка.".into());
    }
    let url = match reqwest::Url::parse(&raw) {
        Ok(u) => u,
        Err(_) => {
            return offline_fail(format!(
                "Чтение страницы не выполнено: «{raw}» — не похоже на корректную ссылку."
            ))
        }
    };
    // Наружу — только TLS (правило web_client). Незашифрованный http не открываем.
    if url.scheme() != "https" {
        return offline_fail(format!(
            "Открываю только https-ссылки, а «{raw}» — {}. Попроси другую ссылку \
             или найди источник через web_search.",
            url.scheme()
        ));
    }
    // Локальные/внутренние адреса и голые IP не открываем: инструмент для сайтов в
    // интернете, а не для служб на этом компьютере или в локальной сети пользователя.
    let host = url.host_str().unwrap_or("").trim_matches(['[', ']']).to_ascii_lowercase();
    if host.is_empty()
        || host == "localhost"
        || host.ends_with(".local")
        || host.ends_with(".internal")
        || host.parse::<std::net::IpAddr>().is_ok()
    {
        return offline_fail(
            "Локальные адреса и IP-адреса не открываю — нужна обычная ссылка на сайт.".into(),
        );
    }

    let client = match web_client() {
        Ok(c) => c,
        Err(e) => return offline_fail(format!("Чтение страницы недоступно: {e}")),
    };

    // С этого момента обращение наружу состоялось (или началось) → went_online: true.
    let online_fail =
        |msg: String| ToolResult { content: msg, sources: Vec::new(), went_online: true };
    let resp = match client.get(url.clone()).send().await {
        Ok(r) => r,
        Err(e) => {
            return online_fail(format!("Не удалось открыть «{raw}»: {}.", friendly_net_err(&e)))
        }
    };
    if !resp.status().is_success() {
        return online_fail(format!(
            "Страница «{raw}» вернула ошибку {} — содержимое недоступно.",
            resp.status()
        ));
    }
    // Читаем только текстовые страницы: PDF/картинки/архивы этому инструменту не по зубам.
    let ctype = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let is_html = ctype.contains("text/html") || ctype.contains("application/xhtml");
    let is_plain = ctype.contains("text/plain");
    if !(ctype.is_empty() || is_html || is_plain) {
        return online_fail(format!(
            "Страница «{raw}» — не текст (тип {ctype}), прочитать её не могу."
        ));
    }
    if let Some(len) = resp.content_length() {
        if len as usize > PAGE_MAX_BYTES {
            return online_fail(format!(
                "Страница «{raw}» слишком большая ({} МБ) — не читаю.",
                len / 1024 / 1024
            ));
        }
    }
    let mut body = match resp.text().await {
        Ok(t) => t,
        Err(e) => return online_fail(format!("Не удалось прочитать «{raw}»: {e}.")),
    };
    // Content-Length мог отсутствовать (chunked) — страхуемся после скачивания.
    if body.len() > PAGE_MAX_BYTES {
        let mut cut = PAGE_MAX_BYTES;
        while !body.is_char_boundary(cut) {
            cut -= 1;
        }
        body.truncate(cut);
    }

    let (title, text) =
        if is_plain { (String::new(), collapse_whitespace(&body)) } else { html_to_text(&body) };
    if text.trim().is_empty() {
        return online_fail(format!(
            "На странице «{raw}» не нашлось читаемого текста (возможно, содержимое \
             подгружается скриптами)."
        ));
    }
    // Предел текста для модели: режем по символам (не байтам) и честно помечаем.
    let (text, truncated) = if text.chars().count() > PAGE_TEXT_MAX_CHARS {
        (text.chars().take(PAGE_TEXT_MAX_CHARS).collect::<String>() + "…", true)
    } else {
        (text, false)
    };
    let shown_title = if title.is_empty() { raw.clone() } else { title.clone() };
    let cut_note = if truncated { " (показано начало — страница длиннее)" } else { "" };
    ToolResult {
        content: format!("Содержимое страницы «{shown_title}» ({raw}){cut_note}:\n\n{text}"),
        sources: vec![WebSource { title: shown_title, url: raw }],
        went_online: true,
    }
}

/// Грубое извлечение читаемого текста из HTML без внешних зависимостей: script/style/
/// noscript выкидываем целиком, блочные теги превращаем в переводы строк, остальные —
/// в пробелы, декодируем частые HTML-сущности и схлопываем пробелы. Это не DOM-парсер,
/// но для «прочитать статью» достаточно, а офлайн-ядру лишние зависимости ни к чему.
/// Возвращает (заголовок из <title>, текст).
fn html_to_text(html: &str) -> (String, String) {
    // <title>…</title> — заголовок страницы (регистронезависимо).
    let title = find_ci(html, "<title", 0)
        .and_then(|start| {
            let open_end = html[start..].find('>').map(|i| start + i + 1)?;
            let close = find_ci(html, "</title", open_end)?;
            Some(collapse_whitespace(&decode_entities(&html[open_end..close])))
        })
        .unwrap_or_default();

    let mut out = String::with_capacity(html.len() / 4);
    let mut i = 0;
    while let Some(rel) = html[i..].find('<') {
        let tag_start = i + rel;
        out.push_str(&html[i..tag_start]); // текст до тега
        // Имя тега (ASCII), чтобы понять: контейнер без текста / блочный / прочий.
        let rest = &html[tag_start + 1..];
        let name: String = rest
            .trim_start_matches('/')
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();
        // script/style/noscript: пропускаем вместе с содержимым до закрывающего тега.
        if !rest.starts_with('/') && matches!(name.as_str(), "script" | "style" | "noscript") {
            let close = format!("</{name}");
            match find_ci(html, &close, tag_start + 1) {
                Some(c) => {
                    i = html[c..].find('>').map(|j| c + j + 1).unwrap_or(html.len());
                    continue;
                }
                None => {
                    i = html.len(); // незакрытый script до конца — дальше текста нет
                    break;
                }
            }
        }
        // Обычный тег: до '>' (грубо; '>' в атрибутах — редкость, переживём).
        let tag_end = match html[tag_start..].find('>') {
            Some(j) => tag_start + j + 1,
            None => {
                i = html.len(); // обрыв разметки — хвост это недописанный тег, не текст
                break;
            }
        };
        // Блочные теги и <br> → перевод строки (сохраняем абзацы/списки/таблицы).
        let block = matches!(
            name.as_str(),
            "p" | "div" | "br" | "li" | "ul" | "ol" | "table" | "tr" | "td" | "th" | "h1" | "h2"
                | "h3" | "h4" | "h5" | "h6" | "section" | "article" | "header" | "footer"
                | "blockquote" | "pre" | "form" | "nav" | "aside" | "main" | "figure"
        );
        out.push(if block { '\n' } else { ' ' });
        i = tag_end;
    }
    out.push_str(&html[i..]); // хвост после последнего тега
    (title, collapse_whitespace(&decode_entities(&out)))
}

/// ASCII-регистронезависимый поиск подстроки (иглы вида "<title", "</script").
/// Байтовый поиск ASCII-иглы в UTF-8 безопасен: байты ASCII не встречаются внутри
/// многобайтовых последовательностей, позиция результата — граница символа.
fn find_ci(haystack: &str, needle: &str, from: usize) -> Option<usize> {
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    if from >= h.len() || n.is_empty() || n.len() > h.len() - from {
        return None;
    }
    (from..=h.len() - n.len()).find(|&i| h[i..i + n.len()].eq_ignore_ascii_case(n))
}

/// Декодирование частых HTML-сущностей (включая числовые &#NNNN; и &#xHH;).
/// Неизвестные сущности оставляем как есть — содержание важнее идеальности.
fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        let tail = &rest[pos..];
        // Сущность: до ';' в ближайших 12 байтах (иначе это просто амперсанд).
        // Ищем по байтам: позиция ASCII-';' — всегда граница символа в UTF-8.
        let semi = tail.bytes().take(12).position(|b| b == b';');
        let Some(semi) = semi else {
            out.push('&');
            rest = &tail[1..];
            continue;
        };
        let ent = &tail[1..semi];
        let decoded: Option<char> = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            "mdash" => Some('—'),
            "ndash" => Some('–'),
            "laquo" => Some('«'),
            "raquo" => Some('»'),
            "hellip" => Some('…'),
            _ => {
                if let Some(num) = ent.strip_prefix("#x").or_else(|| ent.strip_prefix("#X")) {
                    u32::from_str_radix(num, 16).ok().and_then(char::from_u32)
                } else if let Some(num) = ent.strip_prefix('#') {
                    num.parse::<u32>().ok().and_then(char::from_u32)
                } else {
                    None
                }
            }
        };
        match decoded {
            Some(c) => out.push(c),
            None => out.push_str(&tail[..semi + 1]), // неизвестная — как есть
        }
        rest = &tail[semi + 1..];
    }
    out.push_str(rest);
    out
}

/// Схлопнуть пробелы: внутри строк — одиночные пробелы, пустые строки — максимум
/// одна подряд (границы абзацев сохраняются, простыни пробелов — нет).
fn collapse_whitespace(s: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut last_empty = false;
    for line in s.lines() {
        let compact = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if compact.is_empty() {
            if !last_empty && !lines.is_empty() {
                lines.push(String::new());
            }
            last_empty = true;
        } else {
            lines.push(compact);
            last_empty = false;
        }
    }
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_to_text_strips_markup_and_scripts() {
        let html = r#"<html><head><TITLE> Про &laquo;цены&raquo; </TITLE>
            <style>p{color:red}</style></head>
            <body><SCRIPT>var x = "<p>не текст</p>";</SCRIPT>
            <h1>Заголовок</h1>
            <p>Первый&nbsp;абзац &amp; ещё &#8212; вот.</p>
            <div>Второй   <b>жирный</b> текст</div>
            <noscript>Включите JS</noscript></body></html>"#;
        let (title, text) = html_to_text(html);
        assert_eq!(title, "Про «цены»");
        assert!(!text.contains("var x"), "script должен быть выкинут целиком: {text}");
        assert!(!text.contains("color"), "style должен быть выкинут: {text}");
        assert!(!text.contains("Включите JS"), "noscript должен быть выкинут: {text}");
        assert!(text.contains("Заголовок"));
        assert!(text.contains("Первый абзац & ещё — вот."));
        assert!(text.contains("Второй жирный текст"));
        // Блочные теги дали переводы строк — заголовок и абзацы не слиплись.
        assert!(text.lines().count() >= 3, "ожидались отдельные строки: {text:?}");
    }

    #[test]
    fn html_to_text_survives_broken_markup() {
        // Обрыв тега, незакрытый script, сущность без ';' — не паникуем, текст отдаём.
        let (_, text) = html_to_text("Начало <b>жирно</b> &amp конец <img src=");
        assert!(text.contains("Начало"));
        assert!(text.contains("жирно"));
        assert!(text.contains("&amp конец")); // сущность без ';' — оставляем как есть
        let (_, text) = html_to_text("Текст <script>алерт(");
        assert_eq!(text, "Текст");
    }

    #[test]
    fn collapse_whitespace_keeps_paragraphs() {
        let s = "  один   два  \n\n\n\n  три \n\n";
        assert_eq!(collapse_whitespace(s), "один два\n\nтри");
    }
}
