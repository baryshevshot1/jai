// ── Ошибки, понятные интерфейсу ──────────────────────────────────────────────
// Раньше наружу уходила голая строка, и фронтенд угадывал причину сбоя регулярками
// по её тексту. Это ненадёжно (regexp /tim/ ловил «runtime») и хрупко: любая правка
// формулировки в Rust молча меняла поведение интерфейса.
//
// Теперь причину называет тот, кто её знает — код в точке отказа. Наружу уходит
// объект { code, message }: code разбирает интерфейс, message показывает человеку.
//
// Миграция идёт постепенно: команды, чьи сбои интерфейс РАЗЛИЧАЕТ (чат, модели,
// движок, документы), уже отдают AppError; остальные пока возвращают String.
// humanError на фронте понимает оба формата, поэтому порядок перевода свободный.

use serde::Serialize;

/// Код причины сбоя. Значения — часть контракта с интерфейсом (src/util.ts),
/// менять их нельзя без правки обеих сторон.
#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ErrorCode {
    /// Движок не запущен, порт закрыт, соединение отклонено.
    EngineDown,
    /// Движок принял запрос и замолчал дольше допустимого.
    Timeout,
    /// Модели нет в движке (её удалили или не установили).
    ModelMissing,
    /// Не хватает памяти под модель (RAM или VRAM).
    OutOfMemory,
    /// Причина не распознана — интерфейс покажет message как есть.
    Unknown,
}

/// Ошибка, уходящая в интерфейс. `message` — уже человеческий русский текст:
/// интерфейс вправе показать его без перевода, если для кода нет своей формулировки.
#[derive(Serialize, Debug)]
pub(crate) struct AppError {
    pub(crate) code: ErrorCode,
    pub(crate) message: String,
}

impl AppError {
    pub(crate) fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        AppError { code, message: message.into() }
    }

    pub(crate) fn engine_down(message: impl Into<String>) -> Self {
        AppError::new(ErrorCode::EngineDown, message)
    }

    pub(crate) fn timeout(message: impl Into<String>) -> Self {
        AppError::new(ErrorCode::Timeout, message)
    }

    pub(crate) fn unknown(message: impl Into<String>) -> Self {
        AppError::new(ErrorCode::Unknown, message)
    }

    /// Ошибка обращения к движку: connect/timeout различаем по самой ошибке reqwest,
    /// а не по её тексту — reqwest знает причину точно, и она не зависит от локали.
    pub(crate) fn from_reqwest(e: &reqwest::Error, context: &str) -> Self {
        if e.is_timeout() {
            AppError::timeout(format!("{context}: движок не ответил вовремя."))
        } else if e.is_connect() {
            AppError::engine_down(format!("{context}: движок не отвечает."))
        } else {
            AppError::unknown(format!("{context}: {e}"))
        }
    }
}

/// Совместимость с непереведёнными участками: `?` на функции, возвращающей String,
/// продолжает работать. Код при этом честно неизвестен — угадывать его по тексту
/// здесь мы НЕ будем, иначе вернулись бы к разбору строк, только в другом месте.
impl From<String> for AppError {
    fn from(message: String) -> Self {
        AppError::unknown(message)
    }
}

impl From<&str> for AppError {
    fn from(message: &str) -> Self {
        AppError::unknown(message.to_string())
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Результат команды, различающей причины сбоя.
pub(crate) type AppResult<T> = Result<T, AppError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_serializes_as_snake_case_string() {
        // Значения кодов — контракт с интерфейсом: если сериализация изменится,
        // фронтенд молча перестанет узнавать причину и покажет сырой текст.
        let json = serde_json::to_string(&AppError::engine_down("движок молчит")).unwrap();
        assert!(json.contains(r#""code":"engine_down""#), "неожиданный JSON: {json}");
        assert!(json.contains(r#""message":"движок молчит""#), "неожиданный JSON: {json}");

        for (code, expected) in [
            (ErrorCode::Timeout, "timeout"),
            (ErrorCode::ModelMissing, "model_missing"),
            (ErrorCode::OutOfMemory, "out_of_memory"),
            (ErrorCode::Unknown, "unknown"),
        ] {
            let json = serde_json::to_string(&AppError::new(code, "x")).unwrap();
            assert!(json.contains(&format!(r#""code":"{expected}""#)), "{code:?} → {json}");
        }
    }

    #[test]
    fn string_conversion_does_not_guess_the_code() {
        // Угадывание причины по тексту — ровно то, от чего уходим: непереведённый
        // участок обязан честно сказать «не знаю», а не выдать правдоподобную ложь.
        let e: AppError = "connection refused".to_string().into();
        assert_eq!(e.code, ErrorCode::Unknown);
        assert_eq!(e.message, "connection refused");
    }
}
