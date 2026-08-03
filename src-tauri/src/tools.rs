// Онлайн-слой (агентный режим): исходящий интернет-клиент, описания инструментов и
// их исполнители. Это ЕДИНСТВЕННОЕ место, откуда приложение ходит в интернет с
// данными пользователя — и только при явно включённом онлайн-режиме (152-ФЗ).
//
// Изоляция (правило проекта): клиент к Ollama — это `reqwest::Client::new()` на
// `http://127.0.0.1:11434` (localhost, без TLS-рукопожатия). Здесь же — ОТДЕЛЬНЫЙ
// клиент `web_client()` для HTTPS наружу (rustls), с таймаутами и User-Agent.
// Эти два пути не смешиваются: localhost остаётся как был, интернет — явный путь.
//
// Страж исходящих (guard_url): наружу — только https и только публичный интернет.
// Хост резолвится, всё локальное (loopback/private/link-local/CGNAT/ULA) отсекается,
// и та же проверка выполняется на каждом redirect-хопе — публичный сайт не может
// «перенаправить» запрос внутрь этого компьютера или локальной сети (SSRF).
//
// Расширяемость: добавить новый инструмент = +1 запись в `tool_specs()` и +1 ветка
// в `execute_tool()`. Агентный цикл (lib.rs) дёргает инструменты обобщённо по имени
// и не переписывается при добавлении нового.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// User-Agent исходящих запросов: честно представляемся провайдеру (некоторые API
/// отклоняют пустой UA). Версия берётся из Cargo, чтобы не расходилась с пакетом.
const USER_AGENT: &str = concat!("jai/", env!("CARGO_PKG_VERSION"), " (+offline-first assistant)");

/// Таймаут всего запроса наружу: интернет может «висеть», а пользователь ждёт ответа
/// модели — лучше быстро отдать честную ошибку, чем подвесить агентный цикл.
///
/// ВНИМАНИЕ: в reqwest этот потолок покрывает и ЧТЕНИЕ ТЕЛА ответа, а не только
/// установку соединения с ожиданием заголовков. Для коротких ответов инструментов
/// (поиск, страница) это ровно то, что нужно. Для файла в сотни мегабайт — приговор:
/// на канале 4.5 МБ/с за 20 секунд проходит 91 МБ из 465, и загрузка обрывалась
/// ВСЕГДА, у любого пользователя, с текстом «error decoding response body» (Display
/// у reqwest для таймаута в теле). Поэтому большие файлы качает download_client(),
/// у которого общего потолка нет вовсе — см. ниже.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// Отдельный таймаут на установку соединения (нет сети/DNS не резолвится).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
/// Простой ПОТОКА при большой загрузке: столько подряд без единого байта — считаем
/// соединение мёртвым. Ограничивает паузу между кусками, а не длительность загрузки
/// целиком: качать полгигабайта можно сколько угодно долго, молчать — нельзя.
const DOWNLOAD_READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Максимум перенаправлений, за которыми готовы идти: обычным сайтам хватает
/// одного-двух (www, короткие ссылки), длинная цепочка — признак неладного.
const MAX_REDIRECTS: usize = 5;
/// Общий срок чтения страницы вместе с редиректами: REQUEST_TIMEOUT ограничивает
/// каждый запрос по отдельности, а цепочка хопов не должна суммироваться в минуты
/// ожидания — пользователь всё это время смотрит на «Читаю страницу…».
const READ_TOTAL_TIMEOUT: Duration = Duration::from_secs(30);

/// Сколько источников просим у провайдера (максимум Tavily). Широкий охват «разных
/// источников» за один поиск; у advanced-глубины число результатов НЕ влияет на расход
/// кредитов (те же 2 кредита), поэтому ширина бесплатна. Длину сниппетов ограничиваем
/// отдельно (ниже), чтобы 20 источников уместились в контекст модели «за раз».
const MAX_RESULTS: u64 = 20;
/// Глубина поиска Tavily: "advanced" — более полный и разнообразный охват источников,
/// чем "basic" (стоит дороже по кредитам, но даёт заметно лучшую широту).
const SEARCH_DEPTH: &str = "advanced";
/// Предел длины поискового запроса (символы). Запрос уходит стороннему провайдеру —
/// ограничение механически режет объём данных, который туда вообще можно отправить
/// (в т.ч. если инъекция из веб-контента уговорит модель вложить в запрос переписку).
const QUERY_MAX_CHARS: usize = 400;
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
///
/// Редиректы ограничены, и каждый хоп проходит ту же быструю проверку адреса, что и
/// исходный запрос (check_url_shape): сервер не может «перенаправить» нас на
/// localhost или голый IP. DNS-проверку на хопах из синхронной redirect-политики
/// reqwest сделать нельзя — там, где адрес диктует модель (read_url), переходами
/// поэтому управляем вручную (web_client_no_redirects + полный guard_url).
pub fn web_client() -> Result<reqwest::Client, String> {
    web_client_builder()
        .redirect(guarded_redirects())
        .build()
        .map_err(|e| format!("Не удалось создать сетевой клиент: {e}"))
}

/// Клиент для БОЛЬШИХ загрузок (модель распознавания речи — 465 МБ). Отличие от
/// web_client() ровно одно и оно решающее: НЕТ общего таймаута на запрос.
///
/// Почему так, а не «поставить потолок побольше»: любое конечное число здесь — это
/// ставка на скорость канала клиента, а мы её не знаем. 465 МБ на честных 2 Мбит/с —
/// это полчаса, и это нормальная загрузка, а не зависание. Отличать живую загрузку от
/// мёртвой нужно по МОЛЧАНИЮ потока (read_timeout), а не по её общей длительности.
///
/// Страж адресов и запрет http — те же, что у остального онлайн-слоя.
pub(crate) fn download_client() -> Result<reqwest::Client, String> {
    download_builder()
        .https_only(true)
        .build()
        .map_err(|e| format!("Не удалось создать сетевой клиент: {e}"))
}

/// Настройки таймаутов клиента больших загрузок — отдельно от `https_only`.
///
/// Разделено ради теста: проверять поведение таймаутов надо на локальном сервере, а
/// он говорит по http. Если бы тест строил свой клиент «по образцу», он проверял бы
/// копию настроек, а не сами настройки, — и возврат общего `.timeout()` сюда прошёл
/// бы мимо него. Здесь же тест берёт ровно ту заготовку, что уходит в продукт.
/// Запрет http добавляет download_client(), и это проверено отдельным тестом.
fn download_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(DOWNLOAD_READ_TIMEOUT)
        .redirect(guarded_redirects())
}

/// Тот же клиент больших загрузок, но без запрета http — только для тестов против
/// локального сервера. В продукт не уходит (`#[cfg(test)]`).
#[cfg(test)]
pub(crate) fn download_client_plain() -> reqwest::Client {
    download_builder().build().expect("клиент загрузок не собрался")
}

/// Политика перенаправлений: ограниченная длина цепочки, и КАЖДЫЙ хоп проходит
/// быструю проверку адреса. Общая для обычных запросов и для больших загрузок —
/// у второго пути не должно быть послаблений только потому, что файл большой.
fn guarded_redirects() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() > MAX_REDIRECTS {
            return attempt.error("слишком длинная цепочка перенаправлений");
        }
        match check_url_shape(attempt.url()) {
            Ok(()) => attempt.follow(),
            Err(e) => attempt.error(e),
        }
    })
}

/// Клиент для чтения страниц: тот же, но БЕЗ автоматических редиректов — переходами
/// управляет read_url, прогоняя каждый хоп через полный страж адреса с DNS-резолвом.
fn web_client_no_redirects() -> Result<reqwest::Client, String> {
    web_client_builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| format!("Не удалось создать сетевой клиент: {e}"))
}

/// Общая заготовка исходящего клиента: UA, таймауты, только TLS.
fn web_client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(REQUEST_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        .https_only(true)
}

// ── Страж исходящих запросов (SSRF-защита онлайн-слоя) ─────────────────────────

/// Быстрая проверка адреса БЕЗ выхода в сеть: только https, без локальных/служебных
/// имён и без голых IP — инструменты ходят на сайты в интернете, а не в службы на
/// этом компьютере или в локальной сети. Выполняется и на каждом redirect-хопе
/// (политика web_client), поэтому отделена от DNS-части стража (guard_url).
fn check_url_shape(url: &reqwest::Url) -> Result<(), String> {
    if url.scheme() != "https" {
        return Err(format!("открываю только https-ссылки, а это «{}»", url.scheme()));
    }
    let host = url.host_str().unwrap_or("").trim_matches(['[', ']']).to_ascii_lowercase();
    if host.is_empty()
        || host == "localhost"
        || host.ends_with(".localhost")
        || host.ends_with(".local")
        || host.ends_with(".internal")
        || host.parse::<std::net::IpAddr>().is_ok()
    {
        return Err(
            "локальные адреса и IP-адреса не открываю — нужна обычная ссылка на сайт".into()
        );
    }
    Ok(())
}

/// Публичный ли IP: отсекаем всё, что указывает на этот компьютер или в локальную/
/// служебную сеть — loopback, приватные диапазоны, link-local, CGNAT (100.64/10),
/// unique-local и IPv4-адреса, упакованные в IPv6.
fn ip_is_public(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_unspecified()
                || v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || o[0] == 0 // 0.0.0.0/8 «эта сеть»
                || (o[0] == 100 && (64..=127).contains(&o[1]))) // CGNAT 100.64/10
        }
        std::net::IpAddr::V6(v6) => {
            // ::ffff:a.b.c.d — IPv4 внутри IPv6: судим по вложенному IPv4-адресу.
            if let [0, 0, 0, 0, 0, 0xffff, hi, lo] = v6.segments() {
                let v4 = std::net::Ipv4Addr::new(
                    (hi >> 8) as u8,
                    hi as u8,
                    (lo >> 8) as u8,
                    lo as u8,
                );
                return ip_is_public(std::net::IpAddr::V4(v4));
            }
            let s0 = v6.segments()[0];
            !(v6.is_unspecified()
                || v6.is_loopback()
                || (s0 & 0xffc0) == 0xfe80 // link-local fe80::/10
                || (s0 & 0xfe00) == 0xfc00) // unique-local fc00::/7
        }
    }
}

/// Полный страж исходящего адреса: пропускает только https-адреса публичного
/// интернета. Помимо проверки имени (check_url_shape) резолвит хост и отвергает
/// адреса локальных сетей — строковый фильтр легко обойти доменом, чья DNS-запись
/// указывает внутрь. Резолв — блокирующий вызов ОС, поэтому уходит в отдельный
/// поток, не занимая асинхронные воркеры. Наружу при этом уезжает только DNS-запрос
/// с именем хоста (к системному резолверу), содержимое не отправляется — отказ здесь
/// считается «до сети». Повторный резолв внутри самого запроса (окно DNS-rebinding)
/// осознанно принимаем: пиновка адресов не стоит своей сложности для десктопа.
async fn guard_url(url: &reqwest::Url) -> Result<(), String> {
    check_url_shape(url)?;
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    let port = url.port_or_known_default().unwrap_or(443);
    let resolved = tauri::async_runtime::spawn_blocking(move || {
        std::net::ToSocketAddrs::to_socket_addrs(&(host, port))
            .map(|addrs| addrs.collect::<Vec<_>>())
    })
    .await;
    let addrs = match resolved {
        Ok(Ok(a)) => a,
        Ok(Err(_)) | Err(_) => {
            return Err(
                "не удалось определить адрес сайта (возможно, нет интернета или в ссылке \
                 опечатка)"
                    .into(),
            )
        }
    };
    if addrs.is_empty() {
        return Err(
            "не удалось определить адрес сайта (возможно, нет интернета или в ссылке опечатка)"
                .into(),
        );
    }
    // ЛЮБОЙ локальный адрес в ответе DNS — отказ: резолвер мог подмешать внутренний.
    if addrs.iter().any(|a| !ip_is_public(a.ip())) {
        return Err(
            "этот адрес ведёт в локальную сеть, а не в интернет — такие страницы не открываю"
                .into(),
        );
    }
    Ok(())
}

/// Источник из веб-поиска (заголовок + ссылка) — показываем пользователю в чате,
/// как RAG показывает источники из документов. Уходит на фронт через ChatEvent.
/// Deserialize нужен потому, что источники СОХРАНЯЮТСЯ в историю диалога и
/// читаются обратно при открытии: без этого метка «ответ построен на данных из
/// интернета» жила бы ровно до закрытия окна.
#[derive(Clone, Serialize, Deserialize)]
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
/// `final_url` — адрес, с которым РЕАЛЬНО шла работа (после перенаправлений), если
/// обращение к сети было: прозрачность требует показывать, куда ходили на самом деле,
/// а не только куда просили.
pub struct ToolResult {
    pub content: String,
    pub sources: Vec<WebSource>,
    pub went_online: bool,
    pub final_url: Option<String>,
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
            final_url: None,
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
    let offline_fail = |msg: String| ToolResult {
        content: msg,
        sources: Vec::new(),
        went_online: false,
        final_url: None,
    };

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
    // Длинный хвост отрезаем ещё до сети: провайдеру не должна уходить «простыня»
    // (см. QUERY_MAX_CHARS — это и предохранитель от эксфильтрации через запрос).
    let query: String = if query.chars().count() > QUERY_MAX_CHARS {
        query.chars().take(QUERY_MAX_CHARS).collect()
    } else {
        query
    };

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

    // 3) страж адреса провайдера: наружу — только https в публичный интернет.
    // Адрес берётся из настроек, но правило одно для всех исходящих запросов слоя.
    let provider_url = match reqwest::Url::parse(cfg.url.trim()) {
        Ok(u) => u,
        Err(_) => {
            return offline_fail(
                "Веб-поиск не выполнен: адрес поискового провайдера в настройках не похож \
                 на ссылку. Сообщи пользователю, что нужно поправить настройки \
                 (Онлайн-режим)."
                    .into(),
            )
        }
    };
    if let Err(e) = guard_url(&provider_url).await {
        return offline_fail(format!(
            "Веб-поиск не выполнен: {e}. Проверь адрес провайдера в настройках (Онлайн-режим)."
        ));
    }

    // 4) клиент (ошибка сборки — тоже до фактического выхода в сеть)
    let client = match web_client() {
        Ok(c) => c,
        Err(e) => return offline_fail(format!("Веб-поиск недоступен: {e}")),
    };

    // 5) запрос к провайдеру (адаптер по имени провайдера — расширяемо). С этого
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
            final_url: None,
        },
    }
}

/// Исполнитель чтения страницы (read_url). Открывает https-ссылку изолированным
/// клиентом, извлекает читаемый текст (без вёрстки) и отдаёт модели. Все края —
/// честным текстом в `content`: модель перескажет причину пользователю.
async fn read_url(args: &serde_json::Value) -> ToolResult {
    // Отказы ДО сети (кривая ссылка, http, локальный адрес): наружу ничего не ушло.
    let offline_fail = |msg: String| ToolResult {
        content: msg,
        sources: Vec::new(),
        went_online: false,
        final_url: None,
    };

    let raw = args.get("url").and_then(|u| u.as_str()).unwrap_or("").trim().to_string();
    if raw.is_empty() {
        return offline_fail("Чтение страницы не выполнено: пустая ссылка.".into());
    }
    let mut url = match reqwest::Url::parse(&raw) {
        Ok(u) => u,
        Err(_) => {
            return offline_fail(format!(
                "Чтение страницы не выполнено: «{raw}» — не похоже на корректную ссылку."
            ))
        }
    };
    // Страж адреса (guard_url): только https и только публичный интернет — инструмент
    // для сайтов, а не для служб на этом компьютере или в локальной сети пользователя.
    if let Err(e) = guard_url(&url).await {
        return offline_fail(format!(
            "Чтение «{raw}» не выполнено: {e}. Попроси другую ссылку или найди источник \
             через web_search."
        ));
    }
    let requested = url.to_string();

    let client = match web_client_no_redirects() {
        Ok(c) => c,
        Err(e) => return offline_fail(format!("Чтение страницы недоступно: {e}")),
    };

    // С этого момента обращение наружу состоялось (или началось) → went_online: true;
    // в `final_url` — адрес, с которым реально шла работа (для журнала и UI).
    let online_fail = |msg: String, at: &reqwest::Url| ToolResult {
        content: msg,
        sources: Vec::new(),
        went_online: true,
        final_url: Some(at.to_string()),
    };

    // Перенаправления проходим сами (клиент им не следует): каждый хоп — через тот же
    // страж адреса, что и исходная ссылка. Иначе публичный сайт мог бы «перенаправить»
    // чтение на роутер/NAS/службу в локальной сети (SSRF через redirect).
    let deadline = std::time::Instant::now() + READ_TOTAL_TIMEOUT;
    let mut hops = 0usize;
    let mut resp = loop {
        // Новый хоп начинаем только в пределах общего срока (сам запрос ограничен
        // своим REQUEST_TIMEOUT, так что и суммарное ожидание жёстко ограничено).
        if std::time::Instant::now() > deadline {
            return online_fail(
                format!("Не удалось открыть «{raw}»: сайт отвечает слишком долго."),
                &url,
            );
        }
        let r = match client.get(url.clone()).send().await {
            Ok(r) => r,
            Err(e) => {
                return online_fail(
                    format!("Не удалось открыть «{raw}»: {}.", friendly_net_err(&e)),
                    &url,
                )
            }
        };
        if !r.status().is_redirection() {
            break r;
        }
        hops += 1;
        if hops > MAX_REDIRECTS {
            return online_fail(
                format!(
                    "Страница «{raw}» перенаправляет слишком много раз подряд — чтение \
                     остановлено."
                ),
                &url,
            );
        }
        let loc = r
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .trim();
        let next = if loc.is_empty() { None } else { url.join(loc).ok() };
        let Some(next) = next else {
            return online_fail(
                format!(
                    "Страница «{raw}» перенаправляет по некорректному адресу — чтение \
                     остановлено."
                ),
                &url,
            );
        };
        if let Err(e) = guard_url(&next).await {
            return online_fail(
                format!("Страница «{raw}» перенаправляет на закрытый адрес: {e}."),
                &url,
            );
        }
        url = next;
    };
    if !resp.status().is_success() {
        return online_fail(
            format!(
                "Страница «{raw}» вернула ошибку {} — содержимое недоступно.",
                resp.status()
            ),
            &url,
        );
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
        return online_fail(
            format!("Страница «{raw}» — не текст (тип {ctype}), прочитать её не могу."),
            &url,
        );
    }
    // Content-Length (если сервер его прислал) — быстрый отказ без скачивания.
    if let Some(len) = resp.content_length() {
        if len as usize > PAGE_MAX_BYTES {
            return online_fail(
                format!(
                    "Страница «{raw}» слишком большая ({} МБ) — не читаю.",
                    len / 1024 / 1024
                ),
                &url,
            );
        }
    }
    // Тело читаем ПОТОКОВО и обрываем на лимите: Content-Length может отсутствовать
    // (chunked) или врать, а скачивать в память «сколько пришлёт сервер» нельзя —
    // память клиентского железа и так впритык. Хвост сверх лимита просто не качаем.
    let mut buf: Vec<u8> = Vec::new();
    while buf.len() < PAGE_MAX_BYTES {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                let room = PAGE_MAX_BYTES - buf.len();
                buf.extend_from_slice(&chunk[..chunk.len().min(room)]);
            }
            Ok(None) => break,
            Err(e) => {
                return online_fail(
                    format!("Не удалось прочитать «{raw}»: {}.", friendly_net_err(&e)),
                    &url,
                )
            }
        }
    }
    // Байты → строка. Битые байты (и возможный разрез символа ровно на лимите)
    // заменяются, не роняя чтение: содержание важнее идеальности.
    let body = String::from_utf8_lossy(&buf).into_owned();

    let (title, text) =
        if is_plain { (String::new(), collapse_whitespace(&body)) } else { html_to_text(&body) };
    if text.trim().is_empty() {
        return online_fail(
            format!(
                "На странице «{raw}» не нашлось читаемого текста (возможно, содержимое \
                 подгружается скриптами)."
            ),
            &url,
        );
    }
    // Предел текста для модели: режем по символам (не байтам) и честно помечаем.
    let (text, truncated) = if text.chars().count() > PAGE_TEXT_MAX_CHARS {
        (text.chars().take(PAGE_TEXT_MAX_CHARS).collect::<String>() + "…", true)
    } else {
        (text, false)
    };
    let shown_title = if title.is_empty() { raw.clone() } else { title.clone() };
    let cut_note = if truncated { " (показано начало — страница длиннее)" } else { "" };
    // Фактический адрес после перенаправлений показываем, если он отличается от
    // запрошенного: должно быть видно, откуда на самом деле пришёл текст.
    let final_url = url.to_string();
    let addr_note =
        if final_url == requested { String::new() } else { format!(" → {final_url}") };
    ToolResult {
        content: format!(
            "Содержимое страницы «{shown_title}» ({raw}{addr_note}){cut_note}:\n\n{text}"
        ),
        sources: vec![WebSource { title: shown_title, url: final_url.clone() }],
        went_online: true,
        final_url: Some(final_url),
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
    // Куда фактически ушёл запрос (после возможных перенаправлений) — для журнала.
    let final_url = resp.url().to_string();
    let json: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    let answer = json.get("answer").and_then(|a| a.as_str()).unwrap_or("");
    let results = json.get("results").and_then(|r| r.as_array());
    Ok(ToolResult { final_url: Some(final_url), ..format_results(query, answer, results) })
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
    // Куда фактически ушёл запрос (после возможных перенаправлений) — для журнала.
    let final_url = resp.url().to_string();
    let json: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    let answer = json.get("answer").and_then(|a| a.as_str()).unwrap_or("");
    let results = json.get("results").and_then(|r| r.as_array());
    Ok(ToolResult { final_url: Some(final_url), ..format_results(query, answer, results) })
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
    // Фактический адрес знает адаптер провайдера — он и заполняет final_url.
    ToolResult { content, sources, went_online: true, final_url: None }
}

/// Превратить сетевую ошибку reqwest в человеческое объяснение (без интернета /
/// таймаут / прочее) — модель перескажет это пользователю спокойно.
fn friendly_net_err(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "превышено время ожидания ответа".into()
    } else if e.is_connect() {
        "не удалось соединиться (вероятно, нет интернета)".into()
    } else if e.is_redirect() {
        // Нашей redirect-политике не понравился переход: закрытый адрес или цепочка.
        "перенаправление отклонено (страница уводит на недопустимый адрес)".into()
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

    #[test]
    fn ip_is_public_rejects_local_ranges() {
        use std::net::IpAddr;
        let local = [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1", // CGNAT
            "0.0.0.0",
            "255.255.255.255",
            "::1",
            "::",
            "fe80::1",
            "fc00::1",
            "fd12:3456::1",
            "::ffff:192.168.1.1", // IPv4-mapped IPv6
        ];
        for s in local {
            assert!(!ip_is_public(s.parse::<IpAddr>().unwrap()), "{s} должен считаться локальным");
        }
        let public = ["8.8.8.8", "77.88.8.8", "100.128.0.1", "172.32.0.1", "2606:4700::1111"];
        for s in public {
            assert!(ip_is_public(s.parse::<IpAddr>().unwrap()), "{s} должен считаться публичным");
        }
    }

    /// Клиент больших загрузок наружу ходит ТОЛЬКО по TLS. Проверка нужна потому,
    /// что ради тестов у него есть заготовка без этого запрета: она не должна
    /// однажды оказаться тем, что уходит в продукт.
    #[tokio::test]
    async fn download_client_refuses_plain_http() {
        let c = download_client().unwrap();
        let err = c.get("http://example.com/file.bin").send().await.unwrap_err();
        assert!(!err.is_connect(), "запрос вообще не должен был уйти в сеть: {err}");
    }

    #[test]
    fn check_url_shape_filters_local_and_plain_http() {
        let bad = [
            "http://example.com/",
            "https://localhost/admin",
            "https://foo.localhost/",
            "https://nas.local/",
            "https://api.internal/x",
            "https://192.168.1.1/",
            "https://[::1]/",
        ];
        for s in bad {
            let u = reqwest::Url::parse(s).unwrap();
            assert!(check_url_shape(&u).is_err(), "{s} должен быть отвергнут");
        }
        let u = reqwest::Url::parse("https://example.com/page?q=1").unwrap();
        assert!(check_url_shape(&u).is_ok());
    }
}
