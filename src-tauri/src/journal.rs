// ── Журнал приложения: файл, stdout и хвост в памяти ─────────────────────────
//
// Зачем он появился. Отладка голосового ввода встала намертво на просьбе «пришлите
// текст из консоли разработчика»: у заказчика нет ни консоли, ни привычки в неё
// ходить, а в собранном приложении её и не открыть. Диагностика, которая существует
// только в голове разработчика и только на его машине, — это отсутствие диагностики.
//
// Что здесь есть: строка одновременно уходит в файл (переживает перезапуск), в
// stdout (видно при запуске из терминала) и в кольцевой буфер последних записей,
// который показывает экран «Диагностика» — туда пользователь дойти может.
//
// Почему не tauri-plugin-log, который просился сам: экрану «Диагностика» нужен
// именно ХВОСТ последних записей в памяти, а плагин отдаёт только файл — пришлось
// бы читать и разбирать его обратно. Плюс лишняя зависимость (крейт + npm-пакет +
// разрешение в capabilities) в коробочном продукте, который собирается офлайн.
// Здесь та же работа делается без единой новой зависимости.
//
// Персональные данные сюда не пишем: журнал — про работу приложения (что не
// собралось, что не скачалось, где упал старт), а не про переписку пользователя.

use std::collections::VecDeque;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// Сколько последних записей держим в памяти для экрана «Диагностика». Двухсот
/// хватает на весь старт приложения с запасом, а памяти это стоит килобайты.
const TAIL_CAP: usize = 200;

/// Потолок файла журнала. Больше — заводим новый, прежний оставляем как `.1`:
/// коробочный продукт живёт у клиента годами, и журнал не должен съесть диск.
const FILE_CAP: u64 = 2 * 1024 * 1024;

/// Уровень записи. Строкой, а не числом: журнал читают глазами.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Level {
    Info,
    Warn,
    Error,
}

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Level::Info => "INFO",
            Level::Warn => "WARN",
            Level::Error => "ERROR",
        }
    }
}

/// Одна запись журнала. `at_ms` — миллисекунды: формат даты выбирает интерфейс,
/// а не бэкенд (иначе локаль пришлось бы держать в двух местах).
#[derive(Serialize, Clone)]
pub(crate) struct Entry {
    pub(crate) at_ms: i64,
    pub(crate) level: Level,
    /// Откуда запись: "voice", "старт", "ui" и т. п. Помогает читать хвост.
    pub(crate) source: String,
    pub(crate) message: String,
}

struct Journal {
    /// None — путь ещё не известен (до init) либо каталог недоступен. Журнал при
    /// этом продолжает работать в память и stdout: потерять диагностику из-за
    /// недоступного каталога было бы худшим из вариантов.
    path: Option<PathBuf>,
    tail: VecDeque<Entry>,
    /// Сколько байт дописано в текущий файл. Считаем сами, а не спрашиваем ФС на
    /// каждой строке: stat на каждую запись — лишний системный вызов, а нам нужно
    /// лишь понимать, когда пора заводить новый файл.
    written: u64,
}

static JOURNAL: Mutex<Option<Journal>> = Mutex::new(None);

/// Подключить файл журнала. Зовётся один раз при старте приложения, ПОСЛЕ того как
/// стал известен каталог логов. До этого момента записи уже копятся в памяти —
/// поэтому при подключении файла хвост в него и сбрасываем, ничего не теряя.
pub(crate) fn init(app: &tauri::AppHandle) {
    use tauri::Manager;
    let dir = match app.path().app_log_dir() {
        Ok(d) => d,
        Err(e) => {
            write(Level::Warn, "журнал", format!("каталог логов недоступен: {e}"));
            return;
        }
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        write(Level::Warn, "журнал", format!("не создать каталог логов: {e}"));
        return;
    }
    let path = dir.join("jai.log");
    let already = rotate_if_big(&path);

    let backlog = {
        let mut slot = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
        let j = slot.get_or_insert_with(new_journal);
        j.path = Some(path.clone());
        // Учитываем то, что уже лежит в файле от прошлых запусков: иначе счётчик
        // начинался бы с нуля каждый раз и потолок отодвигался бы бесконечно.
        j.written = already;
        j.tail.iter().cloned().collect::<Vec<_>>()
    };
    // Всё, что случилось до появления файла (в том числе ошибки самого раннего
    // старта), должно оказаться в файле — иначе самые интересные записи живут
    // ровно до закрытия окна.
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "──── запуск ────");
        for e in backlog {
            let _ = writeln!(f, "{}", format_line(&e));
        }
    }
}

/// Куда пишется журнал — показываем в «Диагностике», чтобы можно было прислать файл.
#[tauri::command]
pub(crate) fn journal_path() -> Option<String> {
    let slot = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
    slot.as_ref()
        .and_then(|j| j.path.as_ref())
        .map(|p| p.to_string_lossy().into_owned())
}

/// Последние записи — для экрана «Диагностика».
#[tauri::command]
pub(crate) fn journal_tail() -> Vec<Entry> {
    let slot = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
    slot.as_ref().map(|j| j.tail.iter().cloned().collect()).unwrap_or_default()
}

/// Запись из интерфейса: ошибки окна (window.onerror, unhandledrejection) и отказы
/// старта модулей. Без этой команды ошибки фронтенда живут только в консоли
/// разработчика, куда пользователь никогда не заглянет.
#[tauri::command]
pub(crate) fn journal_log(level: Level, source: String, message: String) {
    write(level, source, message);
}

pub(crate) fn info(source: impl Into<String>, message: impl Into<String>) {
    write(Level::Info, source, message);
}

pub(crate) fn warn(source: impl Into<String>, message: impl Into<String>) {
    write(Level::Warn, source, message);
}

pub(crate) fn error(source: impl Into<String>, message: impl Into<String>) {
    write(Level::Error, source, message);
}

fn new_journal() -> Journal {
    Journal { path: None, tail: VecDeque::with_capacity(TAIL_CAP), written: 0 }
}

fn format_line(e: &Entry) -> String {
    format!("{} [{}] {}: {}", e.at_ms, e.level.as_str(), e.source, e.message)
}

/// Общий путь записи. Ошибки самой записи ГЛОТАЕМ намеренно: журнал — вспомогательный
/// механизм, и падение приложения из-за того, что не удалось пожаловаться в лог,
/// было бы издевательством.
fn write(level: Level, source: impl Into<String>, message: impl Into<String>) {
    let entry = Entry {
        at_ms: crate::now_ms(),
        level,
        source: source.into(),
        message: message.into(),
    };
    let line = format_line(&entry);
    // stdout — для запуска из терминала (в том числе --self-check).
    eprintln!("{line}");

    // Ротация решается ЗДЕСЬ, на каждой записи, а не только при старте. Прежде она
    // жила в init(), то есть потолок в 2 МБ был потолком «на момент запуска»: у
    // клиента приложение работает днями без перезапуска, и зациклившийся источник
    // ошибок (обработчик, падающий на каждое нажатие) наполнял бы файл гигабайтами,
    // ни разу не встретив проверку.
    let (path, rotate) = {
        let mut slot = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
        let j = slot.get_or_insert_with(new_journal);
        if j.tail.len() == TAIL_CAP {
            j.tail.pop_front();
        }
        j.tail.push_back(entry);
        j.written += line.len() as u64 + 1; // +1 — перевод строки
        let rotate = j.written > FILE_CAP;
        if rotate {
            j.written = 0;
        }
        (j.path.clone(), rotate)
    };
    if let Some(path) = path {
        if rotate {
            rotate_now(&path);
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            let _ = writeln!(f, "{line}");
        }
    }
}

/// Отложить текущий файл в `.log.1` и начать новый. Прежний остаётся ровно один:
/// два файла по 2 МБ — это потолок, за который журнал не выходит никогда.
fn rotate_now(path: &std::path::Path) {
    let _ = std::fs::rename(path, path.with_extension("log.1"));
}

fn rotate_if_big(path: &std::path::Path) -> u64 {
    match std::fs::metadata(path).map(|m| m.len()) {
        Ok(len) if len > FILE_CAP => {
            rotate_now(path);
            0
        }
        Ok(len) => len,
        Err(_) => 0,
    }
}

/// Развернуть цепочку причин ошибки в одну строку.
///
/// Это ровно тот инструмент, отсутствие которого стоило проекта отладочной сессии:
/// `reqwest::Error` показывает через Display только зонтичное «error decoding
/// response body», а настоящая причина («operation timed out») лежит на два уровня
/// глубже, в `source()`. Пользователю такое не показывают — это идёт в журнал.
pub(crate) fn causes(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut src = e.source();
    while let Some(s) = src {
        out.push_str(" ← ");
        out.push_str(&s.to_string());
        src = s.source();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Смысл `causes` в том, чтобы достать причину ИЗ-ПОД зонтичного текста. Тест
    /// строит ровно такую двухуровневую ошибку, какой оказалась настоящая.
    #[test]
    fn causes_unwraps_the_whole_chain() {
        #[derive(Debug)]
        struct Inner;
        impl std::fmt::Display for Inner {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("operation timed out")
            }
        }
        impl std::error::Error for Inner {}

        #[derive(Debug)]
        struct Outer(Inner);
        impl std::fmt::Display for Outer {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("error decoding response body")
            }
        }
        impl std::error::Error for Outer {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }

        let s = causes(&Outer(Inner));
        assert!(s.contains("error decoding response body"), "{s}");
        assert!(s.contains("operation timed out"), "истинная причина потеряна: {s}");
    }

    /// Хвост не должен расти бесконечно: журнал живёт у клиента месяцами.
    ///
    /// Проверяем ПРИСУТСТВИЕ свежей записи, а не то, что она последняя: журнал
    /// глобален на процесс, а тесты идут параллельно — соседний тест (загрузка)
    /// пишет в него же. Утверждение «наша запись последняя» было бы верным лишь по
    /// удаче, а тест, зелёный по удаче, ничем не лучше отсутствующего.
    #[test]
    fn tail_is_capped() {
        let mark = format!("метка-{:?}", std::thread::current().id());
        for i in 0..(TAIL_CAP + 50) {
            info("тест", format!("{mark} запись {i}"));
        }
        let tail = journal_tail();
        assert!(tail.len() <= TAIL_CAP, "хвост вырос до {}", tail.len());
        let newest = format!("{mark} запись {}", TAIL_CAP + 49);
        assert!(
            tail.iter().any(|e| e.message == newest),
            "свежая запись вытеснена из хвоста"
        );
        // И обратное: самые старые обязаны выпасть, иначе ограничение не работает.
        let oldest = format!("{mark} запись 0");
        assert!(
            !tail.iter().any(|e| e.message == oldest),
            "старая запись осталась — хвост не ограничивается"
        );
    }
}
