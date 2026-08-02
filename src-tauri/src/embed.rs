// ── Эмбеддинги (Фаза B2): bge-m3 через Ollama, батчем ────────────────────────
//
// Запрос строго на localhost тем же reqwest, что и chat_stream (без TLS, офлайн).
// Эндпоинт /api/embed (Ollama ≥ новые версии): поле `input` принимает массив строк
// → батч за один запрос; ответ — `embeddings`: массив векторов в том же порядке.
// Размерность НЕ задаём — она приходит из ответа (длина вектора); фиксируется в схеме.

use serde::Deserialize;

use crate::error::{AppError, AppResult, ErrorCode};

/// Модель эмбеддингов. Тег не указываем — Ollama сам резолвит в `:latest`.
pub const EMBED_MODEL: &str = "bge-m3";

const OLLAMA: &str = "http://127.0.0.1:11434";

/// Сколько держать bge-m3 в памяти после запроса. Короткий срок — чтобы модель
/// (≈0.7 ГБ) не висела в RAM/VRAM рядом с чат-моделью, когда поиск/индексация уже
/// завершены. При индексации таймер сбрасывается на каждом батче, поэтому модель
/// остаётся загруженной всю индексацию и выгружается лишь спустя время после неё.
const EMBED_KEEP_ALIVE: &str = "30s";

/// Потолок ожидания эмбеддинга батча: холодная загрузка bge-m3 плюс CPU-эмбеддинг
/// большого батча на слабом железе. Это не стрим — общий таймаут уместен: зависший
/// движок не должен держать индексацию/поиск бесконечно.
const EMBED_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

#[derive(Deserialize)]
struct EmbedResponse {
    embeddings: Vec<Vec<f32>>,
}

/// Эмбеддинги батчем: на каждый текст — по вектору, в исходном порядке.
/// Пустой вход → пустой выход (без запроса). Размерность определяется из ответа.
pub async fn embed_batch(texts: &[String]) -> AppResult<Vec<Vec<f32>>> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    let body = serde_json::json!({
        "model": EMBED_MODEL,
        "input": texts,
        "keep_alive": EMBED_KEEP_ALIVE, // быстро освобождаем память после использования
        // Модель поиска считаем на ПРОЦЕССОРЕ. Она маленькая (~1.2 ГБ) и вызывается
        // перед каждым ответом, когда в базе есть документы. На видеокарте она
        // занимала бы место рядом с чат-моделью и вытесняла её: движок держит
        // ограниченное число моделей, и каждый вопрос начинался бы с повторной
        // загрузки весов чат-модели с диска — это десятки секунд на ровном месте.
        "options": { "num_gpu": 0 },
    });

    let resp = crate::HTTP
        .post(format!("{OLLAMA}/api/embed"))
        .timeout(EMBED_TIMEOUT)
        .json(&body)
        .send()
        .await
        .map_err(|e| AppError::from_reqwest(&e, "Поиск по документам недоступен"))?;

    // Модель эмбеддингов не установлена — Ollama отвечает 404. Сообщаем понятно,
    // не падая: установку bge-m3 добавим в провижининг моделей отдельным этапом.
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(AppError::new(
            ErrorCode::ModelMissing,
            format!(
                "Модель поиска по документам «{EMBED_MODEL}» не установлена. \
                 Установите её в настройках: «Документы»."
            ),
        ));
    }
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(AppError::unknown(format!(
            "Движок вернул ошибку {status} при поиске по документам: {text}"
        )));
    }

    let parsed: EmbedResponse = resp
        .json()
        .await
        .map_err(|e| AppError::unknown(format!("Не удалось разобрать ответ движка: {e}")))?;

    // Контроль соответствия: на каждый текст ровно один непустой вектор.
    if parsed.embeddings.len() != texts.len() {
        return Err(AppError::unknown(format!(
            "Движок вернул {} векторов вместо {}",
            parsed.embeddings.len(),
            texts.len()
        )));
    }
    let dim = parsed.embeddings.first().map(|v| v.len()).unwrap_or(0);
    if dim == 0 || parsed.embeddings.iter().any(|v| v.len() != dim) {
        return Err(AppError::unknown(
            "Движок вернул некорректные (пустые или разной длины) векторы",
        ));
    }
    Ok(parsed.embeddings)
}

/// Эмбеддинг одного текста (вопрос пользователя при поиске). Размерность — из ответа.
pub async fn embed_one(text: &str) -> AppResult<Vec<f32>> {
    let mut v = embed_batch(std::slice::from_ref(&text.to_string())).await?;
    v.pop().ok_or_else(|| AppError::unknown("Движок вернул пустой ответ на запрос поиска"))
}

/// Установлена ли модель эмбеддингов (для UI: можно ли индексировать/искать).
/// Мягкая проверка через список тегов; сетевые сбои → false, без паники.
pub async fn is_available() -> bool {
    let resp = match crate::HTTP
        .get(format!("{OLLAMA}/api/tags"))
        .timeout(crate::OLLAMA_META_TIMEOUT)
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => r,
        _ => return false,
    };
    let json: serde_json::Value = match resp.json().await {
        Ok(j) => j,
        Err(_) => return false,
    };
    json.get("models")
        .and_then(|m| m.as_array())
        .map(|arr| {
            arr.iter().any(|m| {
                m.get("name")
                    .and_then(|n| n.as_str())
                    // имя в реестре — "bge-m3:latest"; сверяем по префиксу до тега
                    .map(|n| n.split(':').next() == Some(EMBED_MODEL))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Живой smoke-тест B2: требует запущенной Ollama с установленной bge-m3.
    // В CI Ollama нет — помечен #[ignore]; запускать вручную:
    //   cargo test embed::tests::embed_live -- --ignored --nocapture
    #[ignore]
    #[tokio::test]
    async fn embed_live() {
        let texts = vec![
            "договор аренды нежилого помещения".to_string(),
            "прогноз погоды на завтра".to_string(),
        ];
        let vecs = embed_batch(&texts).await.expect("эмбеддинг батчем");
        assert_eq!(vecs.len(), 2, "по вектору на текст");
        assert_eq!(vecs[0].len(), vecs[1].len(), "одинаковая размерность");
        assert!(!vecs[0].is_empty(), "непустой вектор");
        eprintln!("dim = {}", vecs[0].len());

        // Косинусная близость: одинаковые тексты ближе, чем разные по смыслу.
        let cos = |a: &[f32], b: &[f32]| {
            let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
            let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
            let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
            dot / (na * nb)
        };
        let same = embed_one("договор аренды нежилого помещения").await.unwrap();
        let s_same = cos(&vecs[0], &same);
        let s_diff = cos(&vecs[0], &vecs[1]);
        eprintln!("cos(одинаковые)={s_same:.3}  cos(разные)={s_diff:.3}");
        assert!(s_same > s_diff, "семантически близкие должны быть ближе");
    }
}
