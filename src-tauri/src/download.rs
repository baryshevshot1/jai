// ── Докачиваемая загрузка большого файла ─────────────────────────────────────
//
// Почему это отдельный модуль, а не строчки внутри команды голосового ввода.
// Раньше логика загрузки жила прямо в `voice_model_download`, ходила на Hugging Face
// и потому не проверялась НИЧЕМ: чтобы её выполнить, нужен был интернет и полчаса.
// В результате ошибка, из-за которой загрузка обрывалась ВСЕГДА и у ВСЕХ (общий
// таймаут клиента в 20 секунд покрывал и чтение тела — за это время из 465 МБ
// проходило 60–90), прожила в коде до жалобы заказчика.
//
// Здесь та же работа вынесена в функцию, у которой снаружи и адрес, и файл, и
// клиент. Значит, её можно натравить на локальный сервер, который обрывает тело на
// середине, молчит в поток, врёт про длину или не понимает докачку, — и проверить
// поведение за миллисекунды. Тесты внизу файла делают ровно это.
//
// Правила, которые модуль обязан соблюдать:
//   • незавершённая загрузка живёт в `<файл>.part` и НИКОГДА не переименовывается
//     сама — публикацию делает вызывающий, после сверки контрольной суммы;
//   • обрыв не теряет скачанное: следующая попытка продолжает с того же байта;
//   • отмена — тоже пауза, а не потеря: `.part` остаётся на месте;
//   • общего потолка на длительность загрузки нет. Мёртвое соединение отличается
//     от медленного молчанием (read_timeout клиента), а не общим временем.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::error::{AppError, AppResult};

/// Прогресс шлём не чаще, чем раз в столько байт: каждое событие — сообщение в окно.
pub(crate) const PROGRESS_STEP: u64 = 8 * 1024 * 1024;

/// …но и не реже, чем раз в столько времени — даже если байт натекло всего ничего.
///
/// Без этого на медленном канале события шли раз в 8 МиБ, то есть на 2 Мбит/с — раз
/// в полминуты. Интерфейс не мог посчитать по ним ни скорость, ни оставшееся время
/// (двух замеров не набиралось), и человек, которому ждать дольше всех, оставался
/// ровно без той подписи, ради которой всё и делалось.
const PROGRESS_HEARTBEAT: Duration = Duration::from_secs(2);

/// Сколько неудач ПОДРЯД терпим, прежде чем сдаться. Подряд — потому что обрыв,
/// после которого удалось скачать ещё сотню мегабайт, не приближает отказ.
const MAX_ATTEMPTS: u32 = 5;

/// Общий предохранитель числа попыток: счётчик выше сбрасывается при продвижении, и
/// без этого линия, рвущаяся каждые несколько мегабайт, крутила бы цикл вечно.
const MAX_TOTAL_ATTEMPTS: u32 = 50;

/// Паузы перед повторами, нарастающие. Долбить сервер сразу после обрыва
/// бессмысленно: если сеть отвалилась, ей нужно время вернуться.
pub(crate) const RETRY_PAUSES: [Duration; 4] = [
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
    Duration::from_secs(20),
];

pub(crate) enum Outcome {
    Done,
    Cancelled,
}

/// Куда сообщать ход дела: (статус, сделано, всего). Функцией, а не каналом Tauri, —
/// чтобы тест мог подставить свой счётчик, не поднимая приложение.
pub(crate) type Progress<'a> = &'a (dyn Fn(&str, u64, u64) + Send + Sync);

pub(crate) struct Plan<'a> {
    pub(crate) url: &'a str,
    /// Файл незавершённой загрузки. Готовый результат из него делает вызывающий.
    pub(crate) part: &'a Path,
    /// Ожидаемый полный размер. Именно он, а не слова сервера, решает, доехала ли
    /// загрузка: сервер, закрывший соединение на середине, тоже «завершает поток».
    pub(crate) total: u64,
    /// Человеческий текст стадии для прогресса.
    pub(crate) label: &'a str,
    /// Паузы между повторами. Поле, а не константа, потому что это политика, а не
    /// физика: тестам нужны миллисекунды, продукту — секунды.
    pub(crate) retry_pauses: &'a [Duration],
}

/// Байты человеку: в сообщении об обрыве важно не «сколько процентов», а сколько
/// уже скачано — это ответ на единственный вопрос «мне качать всё заново?».
pub(crate) fn mb(bytes: u64) -> String {
    format!("{:.0} МБ", bytes as f64 / 1024.0 / 1024.0)
}

/// Сколько уже лежит в недокачанном файле. Нет файла — ноль, и это не ошибка.
pub(crate) fn part_len(part: &Path) -> u64 {
    std::fs::metadata(part).map(|m| m.len()).unwrap_or(0)
}

/// Сетевая ошибка: человеку — короткая понятная причина, в журнал — ПОЛНАЯ цепочка.
///
/// Ровно на этом разделении здесь однажды и споткнулись: наружу уходил Display
/// ошибки reqwest («error decoding response body»), выглядевший как повреждение
/// данных, а настоящая причина («operation timed out») лежала в source() и никуда не
/// попадала. Теперь человек видит русскую фразу, а разработчик — цепочку в журнале.
fn net_error(what: &str, e: &reqwest::Error) -> AppError {
    crate::journal::error("загрузка", format!("{what}: {}", crate::journal::causes(e)));
    if e.is_timeout() {
        AppError::timeout(format!("{what}: сеть перестала отвечать."))
    } else if e.is_connect() {
        AppError::unknown(format!("{what}: нет связи с интернетом."))
    } else {
        AppError::unknown(format!("{what}: соединение потеряно."))
    }
}

/// Итог одной попытки. «Скачано ли всё» эта функция не решает — решает размер файла
/// на диске, и проверяет его цикл повторов.
enum Fetch {
    Ended,
    Cancelled,
}

/// Одна попытка: дописать в `.part`, начиная с уже имеющихся `have` байт.
async fn attempt(
    client: &reqwest::Client,
    plan: &Plan<'_>,
    have: u64,
    cancel: &AtomicBool,
    progress: Progress<'_>,
) -> AppResult<Fetch> {
    use std::io::Write;

    let mut req = client.get(plan.url);
    if have > 0 {
        req = req.header(reqwest::header::RANGE, format!("bytes={have}-"));
    }
    let mut resp = match crate::with_cancel(req.send(), cancel).await {
        None => return Ok(Fetch::Cancelled),
        Some(r) => r.map_err(|e| net_error("Не удалось начать загрузку", &e))?,
    };

    let status = resp.status();
    if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
        // «Такого диапазона нет»: либо файл уже целиком у нас, либо `.part` длиннее
        // оригинала. Первое проверит цикл по размеру; второе он же исправит,
        // выбросив мусор. Молча стирать здесь нельзя — решение принимает цикл.
        return Ok(Fetch::Ended);
    }
    if !status.is_success() {
        return Err(AppError::unknown(format!(
            "Источник вернул ошибку {status}. Попробуйте позже либо поставьте файл \
             с флешки."
        )));
    }

    // Дописываем ТОЛЬКО если сервер подтвердил диапазон (206). На 200 он прислал файл
    // с начала, и дописывание склеило бы два начала в мусор, который не сошёлся бы по
    // контрольной сумме — после получаса ожидания.
    let resume = have > 0 && status == reqwest::StatusCode::PARTIAL_CONTENT;
    if have > 0 && !resume {
        crate::journal::warn(
            "загрузка",
            format!("источник не поддержал докачку (ответ {status}) — качаем с начала"),
        );
    }
    let mut file = if resume {
        std::fs::OpenOptions::new().create(true).append(true).open(plan.part)
    } else {
        std::fs::OpenOptions::new().create(true).write(true).truncate(true).open(plan.part)
    }
    .map_err(|e| {
        AppError::unknown(format!("Не удалось записать «{}»: {e}", plan.part.display()))
    })?;

    let mut done = if resume { have } else { 0 };
    let mut last_sent = done;
    let mut last_emit = Instant::now();
    progress(plan.label, done, plan.total);

    // Чанки ждём короткими окнами: «Отмена» срабатывает, даже когда сеть молчит.
    // Мёртвое соединение обрывает сам клиент (read_timeout), а не счётчик здесь:
    // общего потолка на длительность загрузки нет и быть не должно.
    loop {
        if cancel.load(Ordering::Relaxed) {
            // `.part` НЕ удаляем: отменённая загрузка — пауза, а не потеря.
            let _ = file.sync_all();
            return Ok(Fetch::Cancelled);
        }
        match tokio::time::timeout(Duration::from_millis(400), resp.chunk()).await {
            Err(_elapsed) => continue, // окно без данных — перепроверяем отмену
            Ok(Ok(Some(chunk))) => {
                file.write_all(&chunk).map_err(|e| {
                    AppError::unknown(format!("Сбой записи на диск (хватает ли места?): {e}"))
                })?;
                done += chunk.len() as u64;
                // Либо натекло достаточно байт, либо прошло достаточно времени: на
                // быстром канале работает первое условие, на медленном — второе.
                if done - last_sent >= PROGRESS_STEP || last_emit.elapsed() >= PROGRESS_HEARTBEAT {
                    last_sent = done;
                    last_emit = Instant::now();
                    progress(plan.label, done, plan.total.max(done));
                }
            }
            Ok(Ok(None)) => {
                let _ = file.sync_all();
                return Ok(Fetch::Ended);
            }
            Ok(Err(e)) => {
                // Записанное сохраняем и синхронизируем: следующая попытка продолжит
                // ровно отсюда, а не с начала.
                let _ = file.sync_all();
                return Err(net_error("Загрузка прервалась", &e));
            }
        }
    }
}

/// Пауза между попытками, прерываемая отменой. false — пользователь отменил.
async fn sleep_cancellable(d: Duration, cancel: &AtomicBool) -> bool {
    let until = Instant::now() + d;
    while Instant::now() < until {
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20).min(d)).await;
    }
    !cancel.load(Ordering::Relaxed)
}

/// Скачать файл целиком, продолжая с места обрыва и повторяя при неудачах.
/// Результат — заполненный `plan.part` размером `plan.total`; проверку контрольной
/// суммы и переименование делает вызывающий.
pub(crate) async fn resumable(
    client: &reqwest::Client,
    plan: &Plan<'_>,
    cancel: &AtomicBool,
    progress: Progress<'_>,
) -> AppResult<Outcome> {
    // `.part` длиннее цели — это не докачка, а мусор (сменился файл в источнике,
    // оборвалась запись). Такое начинаем заново сразу, не спрашивая.
    if part_len(plan.part) > plan.total {
        crate::journal::warn("загрузка", "недокачанный файл длиннее цели — начинаем заново");
        let _ = std::fs::remove_file(plan.part);
    }

    let mut in_a_row: u32 = 0;
    let mut total_tries: u32 = 0;
    loop {
        let before = part_len(plan.part);
        if before >= plan.total {
            return Ok(Outcome::Done);
        }
        if total_tries >= MAX_TOTAL_ATTEMPTS {
            return Err(gave_up(before, plan.total));
        }
        total_tries += 1;

        let outcome = attempt(client, plan, before, cancel, progress).await;
        if let Ok(Fetch::Cancelled) = outcome {
            crate::journal::info("загрузка", format!("отменена на {before} байт"));
            return Ok(Outcome::Cancelled);
        }
        let after = part_len(plan.part);
        if after >= plan.total {
            return Ok(Outcome::Done); // доехали — как бы ни завершилась попытка
        }
        // Сюда попадаем и при ошибке, и при «поток кончился, а файл короткий»: для
        // пользователя это одно и то же — загрузка не доехала.
        match &outcome {
            Err(e) => crate::journal::warn(
                "загрузка",
                format!("попытка {total_tries}: {} (на {after} из {})", e.message, plan.total),
            ),
            Ok(_) => crate::journal::warn(
                "загрузка",
                format!("попытка {total_tries}: поток кончился на {after} из {}", plan.total),
            ),
        }
        // Заметно продвинулись — связь жива, даём полный запас попыток заново.
        if after.saturating_sub(before) >= PROGRESS_STEP {
            in_a_row = 0;
        } else {
            in_a_row += 1;
        }
        if in_a_row >= MAX_ATTEMPTS {
            return Err(gave_up(after, plan.total));
        }

        let pause = plan
            .retry_pauses
            .get(in_a_row as usize)
            .or_else(|| plan.retry_pauses.last())
            .copied()
            .unwrap_or(Duration::from_secs(2));
        progress(
            &format!("Соединение потеряно — продолжим через {} с", pause.as_secs().max(1)),
            after,
            plan.total,
        );
        if !sleep_cancellable(pause, cancel).await {
            return Ok(Outcome::Cancelled);
        }
    }
}

/// Сообщение, когда повторы исчерпаны. Главное в нём — не извинения, а ответ на
/// единственный вопрос пользователя: пропало ли скачанное. Не пропало.
fn gave_up(done: u64, total: u64) -> AppError {
    AppError::unknown(format!(
        "Скачивание прервалось: соединение потеряно на {} из {}. Скачанное сохранено — \
         нажмите «Продолжить», и загрузка пойдёт дальше с этого места. Если интернета \
         нет, поставьте файл с флешки.",
        mb(done),
        mb(total)
    ))
}

// ─────────────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::AtomicU32;
    use std::sync::Arc;

    /// Как сервер ведёт себя на очередном запросе. Каждый вариант — это отказ,
    /// который РЕАЛЬНО случается в интернете и который прежний код не переживал.
    #[derive(Clone, Copy)]
    enum Behave {
        /// Отдать всё честно, с поддержкой Range.
        Full,
        /// Отдать столько байт и молча закрыть соединение (обрыв на середине).
        CutAfter(usize),
        /// Ответить 200 и игнорировать Range (сервер без поддержки докачки).
        IgnoreRange,
        /// Прислать заголовки и замолчать навсегда (зависший поток).
        Hang,
        /// Соврать про длину: обещать больше, чем отдать.
        ShortBody,
        /// Отдавать по чуть-чуть с паузами — медленная линия. Нужна там, где важно
        /// поймать состояние ПОСРЕДИ загрузки: на локальной петле 4 МБ проскакивают
        /// быстрее, чем успевает сработать что-либо ещё.
        Trickle(usize, Duration),
    }

    struct Server {
        port: u16,
        body: Vec<u8>,
        hits: Arc<AtomicU32>,
    }

    /// Крошечный HTTP-сервер на голом TcpListener. Без зависимостей: тянуть в
    /// коробочный офлайн-продукт веб-фреймворк ради тестов — плохой размен.
    fn serve(body: Vec<u8>, script: Vec<Behave>) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = Arc::new(AtomicU32::new(0));
        let body_out = body.clone();
        let hits_out = hits.clone();

        std::thread::spawn(move || {
            for (i, stream) in listener.incoming().enumerate() {
                let Ok(mut stream) = stream else { break };
                let n = hits_out.fetch_add(1, Ordering::SeqCst) as usize;
                let behave = *script.get(i).or_else(|| script.last()).unwrap_or(&Behave::Full);
                let _ = n;

                // Заголовки запроса: нужен только Range.
                let mut buf = [0u8; 2048];
                let read = stream.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..read]).to_string();
                let from = req
                    .lines()
                    .find_map(|l| l.strip_prefix("Range: bytes="))
                    .and_then(|r| r.trim_end_matches('-').parse::<usize>().ok());

                match behave {
                    Behave::Hang => {
                        let _ = stream.write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                                body_out.len()
                            )
                            .as_bytes(),
                        );
                        let _ = stream.flush();
                        std::thread::sleep(Duration::from_secs(30)); // и молчим
                    }
                    Behave::IgnoreRange => {
                        let _ = stream.write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                                body_out.len()
                            )
                            .as_bytes(),
                        );
                        let _ = stream.write_all(&body_out);
                    }
                    Behave::ShortBody => {
                        // Обещаем полную длину, отдаём половину и закрываемся.
                        let _ = stream.write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                                body_out.len()
                            )
                            .as_bytes(),
                        );
                        let _ = stream.write_all(&body_out[..body_out.len() / 2]);
                    }
                    Behave::Trickle(chunk, pause) => {
                        let _ = stream.write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\nAccept-Ranges: bytes\r\nContent-Length: {}\r\n\r\n",
                                body_out.len()
                            )
                            .as_bytes(),
                        );
                        let _ = stream.flush();
                        for piece in body_out.chunks(chunk) {
                            if stream.write_all(piece).is_err() {
                                break;
                            }
                            let _ = stream.flush();
                            std::thread::sleep(pause);
                        }
                    }
                    Behave::Full | Behave::CutAfter(_) => {
                        let start = from.unwrap_or(0).min(body_out.len());
                        let slice = &body_out[start..];
                        let head = if from.is_some() {
                            format!(
                                "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\n\
                                 Content-Range: bytes {}-{}/{}\r\n\r\n",
                                slice.len(),
                                start,
                                body_out.len() - 1,
                                body_out.len()
                            )
                        } else {
                            format!(
                                "HTTP/1.1 200 OK\r\nAccept-Ranges: bytes\r\n\
                                 Content-Length: {}\r\n\r\n",
                                slice.len()
                            )
                        };
                        let _ = stream.write_all(head.as_bytes());
                        let cut = match behave {
                            Behave::CutAfter(n) => n.min(slice.len()),
                            _ => slice.len(),
                        };
                        let _ = stream.write_all(&slice[..cut]);
                    }
                }
                let _ = stream.flush();
            }
        });
        Server { port, body, hits }
    }

    fn client() -> reqwest::Client {
        // https_only здесь нет намеренно: продуктовый клиент ходит только по TLS
        // (это проверено отдельно, в tools.rs), а тестовому серверу TLS ни к чему.
        reqwest::Client::builder()
            .read_timeout(Duration::from_millis(300))
            .build()
            .unwrap()
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir()
            .join(format!("jai-dl-{name}-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Миллисекунды вместо секунд: тесты проверяют поведение повторов, а не терпение.
    const FAST_PAUSES: [Duration; 1] = [Duration::from_millis(5)];

    fn plan<'a>(srv: &Server, part: &'a Path, url: &'a str) -> Plan<'a> {
        Plan {
            url,
            part,
            total: srv.body.len() as u64,
            label: "Тест",
            retry_pauses: &FAST_PAUSES,
        }
    }

    fn payload(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    /// Опорный случай: файл доезжает целиком и байт в байт.
    #[tokio::test]
    async fn downloads_whole_file() {
        let body = payload(300_000);
        let srv = serve(body.clone(), vec![Behave::Full]);
        let dir = tmp("whole");
        let part = dir.join("f.part");
        let url = format!("http://127.0.0.1:{}/f", srv.port);
        let cancel = AtomicBool::new(false);

        let out = resumable(&client(), &plan(&srv, &part, &url), &cancel, &|_, _, _| {}).await;
        assert!(matches!(out, Ok(Outcome::Done)), "загрузка не завершилась");
        assert_eq!(std::fs::read(&part).unwrap(), body, "файл не совпал с источником");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ГЛАВНЫЙ тест этого модуля: соединение рвётся на середине тела.
    ///
    /// Именно так проявлялся отказ у заказчика (только рвал его собственный таймаут
    /// клиента). Прежний код в этом месте УДАЛЯЛ `.part` и возвращал ошибку — то
    /// есть терял всё скачанное. Здесь загрузка обязана продолжиться и доехать.
    #[tokio::test]
    async fn resumes_after_broken_connection() {
        let body = payload(300_000);
        let srv = serve(body.clone(), vec![Behave::CutAfter(100_000), Behave::Full]);
        let dir = tmp("resume");
        let part = dir.join("f.part");
        let url = format!("http://127.0.0.1:{}/f", srv.port);
        let cancel = AtomicBool::new(false);

        let out = resumable(&client(), &plan(&srv, &part, &url), &cancel, &|_, _, _| {}).await;
        assert!(matches!(out, Ok(Outcome::Done)), "докачка не довела дело до конца");
        assert_eq!(std::fs::read(&part).unwrap(), body, "докачанный файл побит");
        assert!(srv.hits.load(Ordering::SeqCst) >= 2, "докачка не запрашивалась повторно");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Источник не понимает докачку и на Range отвечает целым файлом с начала.
    /// Дописать такое в конец `.part` — значит склеить два начала в мусор, который
    /// провалит проверку суммы после получаса ожидания. Должно перезаписаться.
    #[tokio::test]
    async fn restarts_when_server_ignores_range() {
        let body = payload(200_000);
        let srv = serve(body.clone(), vec![Behave::CutAfter(50_000), Behave::IgnoreRange]);
        let dir = tmp("norange");
        let part = dir.join("f.part");
        let url = format!("http://127.0.0.1:{}/f", srv.port);
        let cancel = AtomicBool::new(false);

        let out = resumable(&client(), &plan(&srv, &part, &url), &cancel, &|_, _, _| {}).await;
        assert!(matches!(out, Ok(Outcome::Done)), "загрузка не завершилась");
        assert_eq!(
            std::fs::read(&part).unwrap(),
            body,
            "склеили начало с началом вместо перезаписи"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Сервер прислал заголовки и замолчал. Загрузка обязана закончиться ошибкой за
    /// разумное время (read_timeout клиента), а не висеть «в процессе» вечно.
    #[tokio::test]
    async fn gives_up_on_silent_stream() {
        let body = payload(200_000);
        let srv = serve(body.clone(), vec![Behave::Hang]);
        let dir = tmp("hang");
        let part = dir.join("f.part");
        let url = format!("http://127.0.0.1:{}/f", srv.port);
        let cancel = AtomicBool::new(false);

        let started = Instant::now();
        let out = resumable(&client(), &plan(&srv, &part, &url), &cancel, &|_, _, _| {}).await;
        assert!(out.is_err(), "молчащий поток принят за успешную загрузку");
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "ждали молчания слишком долго: {:?}",
            started.elapsed()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Сервер соврал про длину и отдал меньше обещанного. «Поток закончился» — не
    /// то же самое, что «файл скачан»: полнота определяется размером, а не словами.
    #[tokio::test]
    async fn short_body_is_not_success() {
        let body = payload(200_000);
        let srv = serve(body.clone(), vec![Behave::ShortBody]);
        let dir = tmp("short");
        let part = dir.join("f.part");
        let url = format!("http://127.0.0.1:{}/f", srv.port);
        let cancel = AtomicBool::new(false);

        let out = resumable(&client(), &plan(&srv, &part, &url), &cancel, &|_, _, _| {}).await;
        assert!(out.is_err(), "недокачанный файл выдан за успешный");
        assert!(
            part_len(&part) < body.len() as u64,
            "размер недокачанного файла не должен дотягивать до полного"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Отмена — это пауза, а не потеря: скачанное остаётся, чтобы продолжить позже.
    /// Прежний код в этом месте удалял `.part`, и отменённая на 400-м мегабайте
    /// загрузка начиналась потом с нуля.
    ///
    /// Сервер отдаёт по кусочку с паузами: на локальной петле «обычные» 4 МБ
    /// проскакивают быстрее, чем успевает взвестись флаг, и тест мерил бы удачу.
    #[tokio::test]
    async fn cancel_keeps_partial_file() {
        let body = payload(300_000);
        let srv = serve(
            body.clone(),
            vec![Behave::Trickle(25_000, Duration::from_millis(120))],
        );
        let dir = tmp("cancel");
        let part = dir.join("f.part");
        let url = format!("http://127.0.0.1:{}/f", srv.port);
        let cancel = Arc::new(AtomicBool::new(false));

        // Отмену взводим по ФАКТУ, а не по часам: ждём, пока на диск лягут первые
        // байты, и только тогда нажимаем «Отмена».
        //
        // Первая версия спала фиксированные 250 мс — и мигала: под нагрузкой (а гейт
        // как раз и запускает тесты после сборки) первый кусок не успевал прийти,
        // отмена срабатывала на пустом файле, и проверка «скачано больше нуля»
        // падала. Тест, который иногда красный без причины, приучает перезапускать
        // прогон вместо того, чтобы читать его, — и однажды так пропустят настоящий
        // отказ. Здесь ждать нечего: условие проверяемо напрямую.
        let flag = cancel.clone();
        let watched = part.clone();
        std::thread::spawn(move || {
            let until = Instant::now() + Duration::from_secs(5); // предохранитель
            while Instant::now() < until && part_len(&watched) == 0 {
                std::thread::sleep(Duration::from_millis(5));
            }
            flag.store(true, Ordering::Relaxed);
        });
        // read_timeout здесь не должен срабатывать раньше отмены — берём щедрый.
        let slow = reqwest::Client::builder()
            .read_timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let out = resumable(&slow, &plan(&srv, &part, &url), &cancel, &|_, _, _| {}).await;
        assert!(matches!(out, Ok(Outcome::Cancelled)), "отмена не сработала");
        assert!(part.exists(), "отмена стёрла скачанное — продолжить будет нечем");
        let got = part_len(&part);
        assert!(
            got > 0 && got < body.len() as u64,
            "ожидали частично скачанный файл, а получили {got} из {}",
            body.len()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ══ РЕГРЕССИОННЫЙ ТЕСТ ПЕРВОПРИЧИНЫ ═══════════════════════════════════════
    ///
    /// Загрузка модели распознавания речи обрывалась ВСЕГДА и у ВСЕХ. Причина:
    /// клиент онлайн-слоя имел `.timeout(20 c)` на весь запрос, а в reqwest этот
    /// потолок покрывает и чтение тела. 465 МБ за 20 секунд — это 24 МБ/с; на
    /// обычном канале проходило 60–90 МБ, после чего reqwest возвращал ошибку с
    /// текстом «error decoding response body», в котором ничего про таймаут нет.
    ///
    /// Тест берёт НАСТОЯЩУЮ заготовку клиента загрузок (tools::download_client_plain)
    /// и сравнивает её с той же заготовкой, которой вернули общий таймаут. Первая
    /// обязана дотащить медленный файл, вторая — провалиться. Если однажды `.timeout()`
    /// вернётся в путь больших загрузок, здесь станет красно.
    #[tokio::test]
    async fn no_total_timeout_on_the_download_path() {
        // Файл идёт кусочками ~600 мс суммарно — «медленная линия» в миниатюре.
        let body = payload(120_000);
        let srv = serve(
            body.clone(),
            vec![Behave::Trickle(20_000, Duration::from_millis(100))],
        );
        let url = format!("http://127.0.0.1:{}/f", srv.port);
        let cancel = AtomicBool::new(false);

        // 1. Так было: общий таймаут короче времени загрузки — обрыв в теле.
        let dir_old = tmp("regress-old");
        let part_old = dir_old.join("f.part");
        let with_total_timeout = reqwest::Client::builder()
            .timeout(Duration::from_millis(250))
            .build()
            .unwrap();
        let old = resumable(
            &with_total_timeout,
            &plan(&srv, &part_old, &url),
            &cancel,
            &|_, _, _| {},
        )
        .await;
        assert!(
            old.is_err(),
            "общий таймаут обязан рвать медленную загрузку — иначе тест не воспроизводит отказ"
        );

        // 2. Так стало: тот же путь, но настройками из продукта.
        let dir_new = tmp("regress-new");
        let part_new = dir_new.join("f.part");
        let out = resumable(
            &crate::tools::download_client_plain(),
            &plan(&srv, &part_new, &url),
            &cancel,
            &|_, _, _| {},
        )
        .await;
        assert!(
            matches!(out, Ok(Outcome::Done)),
            "клиент больших загрузок не дотащил медленный файл — вернулся общий таймаут?"
        );
        assert_eq!(std::fs::read(&part_new).unwrap(), body);

        let _ = std::fs::remove_dir_all(&dir_old);
        let _ = std::fs::remove_dir_all(&dir_new);
    }

    /// На медленном канале прогресс обязан приходить ПО ВРЕМЕНИ, а не только по
    /// набежавшим байтам. Порог был один — 8 МиБ; на 2 Мбит/с это событие раз в
    /// полминуты, интерфейс не успевал набрать двух замеров и не показывал ни
    /// скорости, ни оставшегося времени — ровно тому, кто ждёт дольше всех.
    ///
    /// Здесь весь файл (100 КБ) на два порядка меньше байтового порога: если бы
    /// событие зависело только от него, отчётов о ходе дела было бы ноль.
    #[tokio::test]
    async fn progress_ticks_by_time_on_a_slow_line() {
        let body = payload(100_000);
        let srv = serve(
            body.clone(),
            vec![Behave::Trickle(10_000, Duration::from_millis(700))],
        );
        let dir = tmp("heartbeat");
        let part = dir.join("f.part");
        let url = format!("http://127.0.0.1:{}/f", srv.port);
        let cancel = AtomicBool::new(false);

        let seen = Arc::new(std::sync::Mutex::new(Vec::<u64>::new()));
        let sink = seen.clone();
        let slow = reqwest::Client::builder()
            .read_timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let out = resumable(&slow, &plan(&srv, &part, &url), &cancel, &move |_, done, _| {
            sink.lock().unwrap().push(done);
        })
        .await;
        assert!(matches!(out, Ok(Outcome::Done)), "загрузка не завершилась");

        let ticks = seen.lock().unwrap().clone();
        // Первое событие — стартовое (0 байт); дальше должны идти отчёты по времени.
        let moving: Vec<u64> = ticks.iter().copied().filter(|b| *b > 0).collect();
        assert!(
            moving.len() >= 2,
            "за {:?} пришло всего {} отчётов о ходе дела ({:?}) — скорость посчитать не из чего",
            Duration::from_millis(700 * 10),
            moving.len(),
            ticks
        );
        assert!(
            moving.windows(2).all(|w| w[1] >= w[0]),
            "прогресс не должен идти назад: {moving:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ЖИВАЯ проверка против настоящего источника. В гейт НЕ входит (`#[ignore]`):
    /// требует интернета и качает 465 МБ — у клиента интернета нет, и делать такую
    /// проверку обязательной значит сделать гейт неработающим в целевой среде.
    ///
    /// Проверяет то, что локальный сервер подтвердить не может: что Hugging Face и
    /// его CDN действительно отдают 206 на наш заголовок Range, что докачка после
    /// РЕАЛЬНОГО обрыва склеивается верно, и что sha256 сходится байт в байт.
    ///
    ///   cargo test --manifest-path src-tauri/Cargo.toml live_resume -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn live_resume_from_real_source() {
        use sha2::{Digest, Sha256};
        let dir = tmp("live");
        let part = dir.join("model.part");
        let url = crate::voice::MODEL_URL;
        let total = crate::voice::MODEL_BYTES;
        let client = crate::tools::download_client().unwrap();
        let plan = Plan {
            url,
            part: &part,
            total,
            label: "Живая проверка",
            retry_pauses: &FAST_PAUSES,
        };

        // 1. Качаем начало и обрываем — ровно так, как рвётся живое соединение.
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(6));
            flag.store(true, Ordering::Relaxed);
        });
        let first = resumable(&client, &plan, &cancel, &|_, _, _| {}).await;
        assert!(matches!(first, Ok(Outcome::Cancelled)), "не удалось оборвать загрузку");
        let got = part_len(&part);
        assert!(got > 0 && got < total, "оборвали не посередине: {got} из {total}");
        eprintln!("оборвано на {got} байт из {total} — продолжаем");

        // 2. Продолжаем с места обрыва. Если CDN не поддержит Range или мы склеим
        //    неверно — контрольная сумма ниже не сойдётся.
        let go = AtomicBool::new(false);
        let out = resumable(&client, &plan, &go, &|_, done, all| {
            eprintln!("  {done} из {all}");
        })
        .await;
        assert!(matches!(out, Ok(Outcome::Done)), "докачка не завершилась: {:?}", out.err());
        assert_eq!(part_len(&part), total, "размер докачанного не совпал");

        let mut f = std::fs::File::open(&part).unwrap();
        let mut h = Sha256::new();
        let mut buf = vec![0u8; 1024 * 1024];
        loop {
            let n = std::io::Read::read(&mut f, &mut buf).unwrap();
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
        }
        let digest = crate::provision::hex_digest(h);
        assert_eq!(
            digest,
            crate::voice::MODEL_SHA256,
            "докачанный файл не совпал с эталоном — склейка по Range неверна"
        );
        eprintln!("sha256 сошлась: {digest}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Мусор длиннее цели (сменился файл в источнике) не должен «докачиваться».
    #[tokio::test]
    async fn oversized_partial_is_discarded() {
        let body = payload(100_000);
        let srv = serve(body.clone(), vec![Behave::Full]);
        let dir = tmp("oversize");
        let part = dir.join("f.part");
        std::fs::write(&part, vec![0u8; 500_000]).unwrap();
        let url = format!("http://127.0.0.1:{}/f", srv.port);
        let cancel = AtomicBool::new(false);

        let out = resumable(&client(), &plan(&srv, &part, &url), &cancel, &|_, _, _| {}).await;
        assert!(matches!(out, Ok(Outcome::Done)), "загрузка не завершилась");
        assert_eq!(std::fs::read(&part).unwrap(), body, "мусор не был выброшен");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
