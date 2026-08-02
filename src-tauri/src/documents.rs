// ── Документы: извлечение текста, база знаний (RAG), поиск ──────────────────
// Фаза A — вытащить текст из файла (PDF/DOCX/TXT) и прикрепить картинку к вопросу.
// Фаза B — индексация в векторную базу (docstore) и семантический поиск по ней.

use serde::Serialize;
use sha2::{Digest, Sha256};
use tauri::ipc::Channel;

use crate::error::{AppError, AppResult};
use crate::{chunk, docstore, embed, now_ms};

/// Установлена ли модель эмбеддингов (для интерфейса базы документов): можно ли
/// индексировать и искать. Мягкая проверка, сетевые сбои → false.
#[tauri::command]
pub(crate) async fn embedding_status() -> bool {
    embed::is_available().await
}

// ── Документы (Фаза A): извлечение текста из одного файла ────────────────────

/// Результат извлечения текста из документа.
#[derive(Serialize)]
pub(crate) struct DocumentText {
    name: String,
    ext: String,
    text: String,
    chars: usize,
}

/// Извлекает текст из файла по пути. Тип — по расширению. Чтение только в Rust.
/// Синхронная команда: тяжёлый разбор (крупный PDF) идёт в пуле потоков Tauri.
#[tauri::command]
pub(crate) fn extract_document(path: String) -> Result<DocumentText, String> {
    read_document(&path)
}

/// Читает изображение и возвращает СЫРОЙ base64 (без префикса `data:`) для вложения
/// в сообщение (поле `images` у vision-моделей). Проверяет тип и размер; чтение — в Rust.
#[tauri::command]
pub(crate) fn read_image_base64(path: String) -> Result<String, String> {
    use base64::Engine;
    let p = std::path::Path::new(&path);
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .unwrap_or_default();
    if !matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "webp" | "gif") {
        return Err("Формат не поддерживается. Доступны: PNG, JPG, WEBP, GIF".to_string());
    }
    let bytes = std::fs::read(p).map_err(|e| format!("Не удалось прочитать изображение: {e}"))?;
    if bytes.is_empty() {
        return Err("Файл пуст".to_string());
    }
    // Картинка занимает контекст vision-модели — ограничиваем размер во избежание
    // переполнения num_ctx и тяжёлых запросов. Очень большие — понятная ошибка.
    const MAX: usize = 12 * 1024 * 1024;
    if bytes.len() > MAX {
        return Err(format!(
            "Слишком большое изображение ({} МБ). Максимум — 12 МБ; уменьшите файл.",
            bytes.len() / 1024 / 1024
        ));
    }
    let bytes = normalize_attached_image(bytes, &ext)?;
    Ok(base64::engine::general_purpose::STANDARD.encode(&bytes))
}

/// Привести прикреплённое изображение к формату, который движок Ollama гарантированно
/// прочитает. PNG/JPEG/GIF движок понимает — байты не трогаем (быстро и без потерь).
/// WebP он НЕ умеет (llama.cpp/stb_image: «Failed to load image or audio file»),
/// поэтому декодируем у себя и перекодируем: с прозрачностью → PNG, без — JPEG 85
/// (PNG для фотографий раздувался бы в разы). Огромные кадры заодно ужимаем до
/// 2048 px по длинной стороне: vision-модели всё равно приводят вход к ~1 Мп,
/// а нам меньше гонять байтов в движок.
fn normalize_attached_image(bytes: Vec<u8>, ext: &str) -> Result<Vec<u8>, String> {
    if ext != "webp" {
        return Ok(bytes);
    }
    let img = image::load_from_memory(&bytes)
        .map_err(|_| "Не удалось прочитать WEBP-изображение (файл повреждён?)".to_string())?;
    const MAX_SIDE: u32 = 2048;
    let img = if img.width().max(img.height()) > MAX_SIDE {
        img.resize(MAX_SIDE, MAX_SIDE, image::imageops::FilterType::Triangle)
    } else {
        img
    };
    let mut out = std::io::Cursor::new(Vec::new());
    if img.color().has_alpha() {
        img.write_to(&mut out, image::ImageFormat::Png)
    } else {
        let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85);
        img.write_with_encoder(enc)
    }
    .map_err(|e| format!("Не удалось перекодировать изображение: {e}"))?;
    Ok(out.into_inner())
}

/// Извлечение текста из файла. Переиспользуется индексацией базы (Фаза B3),
/// поэтому вынесено из команды в обычную функцию.
fn read_document(path: &str) -> Result<DocumentText, String> {
    let p = std::path::Path::new(path);
    if !p.is_file() {
        return Err("Файл не найден или недоступен".to_string());
    }
    let name = p
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("документ")
        .to_string();
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .unwrap_or_default();

    let text = match ext.as_str() {
        "txt" | "md" => extract_txt(p)?,
        "pdf" => extract_pdf(p)?,
        "docx" => extract_docx(p)?,
        _ => return Err("Формат не поддерживается. Доступны: PDF, Word, текст".to_string()),
    };

    let text = text.trim().to_string();
    if text.chars().filter(|c| !c.is_whitespace()).count() < 5 {
        return Err("Не удалось извлечь текст: файл пуст или без текстового слоя".to_string());
    }
    let chars = text.chars().count();
    Ok(DocumentText { name, ext, text, chars })
}

// TXT/MD: читаем как UTF-8 с заменой битых байтов (без паники).
fn extract_txt(p: &std::path::Path) -> Result<String, String> {
    let bytes = std::fs::read(p).map_err(|e| format!("Не удалось прочитать файл: {e}"))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

// PDF: текстовый слой через pdf-extract (lopdf, чистый Rust). Скан → понятная ошибка.
fn extract_pdf(p: &std::path::Path) -> Result<String, String> {
    let bytes = std::fs::read(p).map_err(|e| format!("Не удалось прочитать файл: {e}"))?;
    // pdf-extract на экзотике (нестандартные кодировки, битые структуры) паникует,
    // а не возвращает Err — без перехвата команда обрывается молча и кнопка
    // прикрепления «крутится» вечно. Ловить безопасно: байты переданы по значению,
    // повреждённого общего состояния после паники не остаётся.
    let text = std::panic::catch_unwind(|| pdf_extract::extract_text_from_mem(&bytes))
        .map_err(|_| "Не удалось разобрать PDF: файл в неподдерживаемом формате".to_string())?
        .map_err(|e| format!("Не удалось разобрать PDF: {e}"))?;
    if text.chars().filter(|c| !c.is_whitespace()).count() < 20 {
        return Err(
            "Похоже, это PDF-скан (изображение без текстового слоя). \
             Распознавание текста (OCR) появится в этапе по зрению."
                .to_string(),
        );
    }
    Ok(text)
}

// DOCX: это ZIP; берём word/document.xml и собираем текст из <w:t>, перенос на </w:p>.
fn extract_docx(p: &std::path::Path) -> Result<String, String> {
    use quick_xml::events::Event;
    use std::io::Read;

    let file = std::fs::File::open(p).map_err(|e| format!("Не удалось открыть файл: {e}"))?;
    let mut zip = zip::ZipArchive::new(file).map_err(|_| "Повреждённый файл DOCX".to_string())?;
    // XML сжимается deflate в сотни раз, поэтому лимитируем именно РАСПАКОВАННЫЙ
    // размер: иначе крафтовый файл в мегабайт (zip-бомба) развернулся бы в гигабайты
    // прямо в памяти. Реальные документы помещаются в лимит с огромным запасом.
    // Заявленный размер в заголовке может врать — take() ограничивает и фактический.
    const MAX_XML_BYTES: u64 = 64 * 1024 * 1024;
    let entry = zip
        .by_name("word/document.xml")
        .map_err(|_| "Это не похоже на документ Word".to_string())?;
    if entry.size() > MAX_XML_BYTES {
        return Err("Документ слишком велик для обработки".to_string());
    }
    let mut xml = String::new();
    entry
        .take(MAX_XML_BYTES)
        .read_to_string(&mut xml)
        .map_err(|e| format!("Ошибка чтения DOCX: {e}"))?;

    let mut reader = quick_xml::Reader::from_str(&xml);
    let mut out = String::new();
    let mut in_text = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) if e.name().as_ref() == b"w:t" => in_text = true,
            Ok(Event::End(e)) if e.name().as_ref() == b"w:t" => in_text = false,
            Ok(Event::End(e)) if e.name().as_ref() == b"w:p" => out.push('\n'),
            Ok(Event::Text(t)) if in_text => {
                if let Ok(decoded) = t.decode() {
                    match quick_xml::escape::unescape(&decoded) {
                        Ok(u) => out.push_str(&u),
                        Err(_) => out.push_str(&decoded),
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("Ошибка разбора DOCX: {e}")),
            _ => {}
        }
    }
    Ok(out)
}

// ── База документов (Фаза B): индексация, список, удаление ────────────────────

/// Прогресс индексации, шлётся в окно через Channel (как стриминг чата).
/// phase: "chunk" | "embed" | "store" | "done".
#[derive(Clone, Serialize)]
pub(crate) struct IndexProgress {
    phase: String,
    current: usize,
    total: usize,
}

/// Итог индексации одного документа.
/// status: "indexed" — добавлен; "exists" — уже был (тот же sha256), не дублируем;
/// "reindexed" — база была пересоздана из-за смены размерности вектора.
#[derive(Serialize)]
pub(crate) struct IndexResult {
    status: String,
    document: docstore::DocumentMeta,
    rebuilt: bool,
}

/// Индексация документа в базу: sha256 → извлечение текста (Фаза A) → чанкинг →
/// эмбеддинги батчами (с прогрессом) → запись в транзакции. Идемпотентно по sha256.
#[tauri::command]
pub(crate) async fn index_document(
    app: tauri::AppHandle,
    path: String,
    project_id: Option<String>,
    on_progress: Channel<IndexProgress>,
) -> AppResult<IndexResult> {
    // 1–2) sha256 + извлечение текста + чанкинг: на крупном PDF это секунды–минуты
    // процессора и диска — уводим из async-воркера в пул блокирующих потоков, чтобы
    // не подвешивать стрим чата и остальные команды. Паника разборщика приходит как
    // ошибка join — превращаем в понятный текст, а не в вечно висящий промис.
    let (sha, doc, chunks) = {
        let path = path.clone();
        tauri::async_runtime::spawn_blocking(move || {
            let bytes =
                std::fs::read(&path).map_err(|e| format!("Не удалось прочитать файл: {e}"))?;
            let mut hasher = Sha256::new();
            hasher.update(&bytes);
            let sha: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
            drop(bytes); // hash посчитан — не держим весь файл, пока идёт разбор
            let doc = read_document(&path)?;
            let chunks = chunk::chunk_text(&doc.text);
            Ok::<_, String>((sha, doc, chunks))
        })
        .await
        .map_err(|e| format!("Не удалось разобрать документ: {e}"))??
    };
    if chunks.is_empty() {
        return Err(AppError::unknown("Не удалось разбить документ на фрагменты"));
    }
    let total = chunks.len();
    let _ = on_progress.send(IndexProgress {
        phase: "chunk".into(),
        current: total,
        total,
    });

    // 3) дубликат по sha256 — короткая сессия БД, соединение не держим через await.
    let db = docstore::db_path(&app)?;
    let existing = {
        let db = db.clone();
        let sha = sha.clone();
        let pid = project_id.clone();
        tauri::async_runtime::spawn_blocking(move || {
            let conn = docstore::open(&db)?;
            docstore::find_by_hash(&conn, &sha, pid.as_deref())
        })
        .await
        .map_err(|e| format!("Сбой проверки дубликата: {e}"))??
    };
    if let Some(existing) = existing {
        return Ok(IndexResult {
            status: "exists".into(),
            document: existing,
            rebuilt: false,
        });
    }

    // 4) эмбеддинги батчами — самый долгий этап, шлём прогресс по каждому батчу.
    const BATCH: usize = 16;
    let mut vectors: Vec<Vec<f32>> = Vec::with_capacity(total);
    for batch in chunks.chunks(BATCH) {
        let part = embed::embed_batch(batch).await?;
        vectors.extend(part);
        let _ = on_progress.send(IndexProgress {
            phase: "embed".into(),
            current: vectors.len(),
            total,
        });
    }
    let dim = vectors[0].len();

    // 5) запись: документ + фрагменты + векторы одной транзакцией (атомарно) — тоже
    // блокирующая работа (SQLite, сотни векторов), в пуле потоков.
    let added_at = now_ms();
    let (doc_id, rebuilt, doc) = {
        let pid = project_id.clone();
        tauri::async_runtime::spawn_blocking(move || {
            let mut conn = docstore::open(&db)?;
            let rebuilt = docstore::ensure_vec_table(&conn, dim)?;
            let tx = conn
                .transaction()
                .map_err(|e| format!("Не удалось начать транзакцию: {e}"))?;
            let doc_id = docstore::insert_document(
                &tx,
                &doc.name,
                &doc.ext,
                &sha,
                added_at,
                doc.chars as i64,
                total as i64,
                pid.as_deref(),
            )?;
            for (i, (text, vec)) in chunks.iter().zip(&vectors).enumerate() {
                let chunk_id = docstore::insert_chunk(&tx, doc_id, i as i64, text, None)?;
                docstore::insert_vector(&tx, chunk_id, vec)?;
            }
            tx.commit()
                .map_err(|e| format!("Не удалось сохранить индекс: {e}"))?;
            Ok::<_, String>((doc_id, rebuilt, doc))
        })
        .await
        .map_err(|e| format!("Сбой записи в базу: {e}"))??
    };
    let _ = on_progress.send(IndexProgress {
        phase: "done".into(),
        current: total,
        total,
    });

    Ok(IndexResult {
        status: if rebuilt { "reindexed" } else { "indexed" }.into(),
        document: docstore::DocumentMeta {
            id: doc_id,
            filename: doc.name,
            ext: doc.ext,
            added_at,
            char_count: doc.chars as i64,
            chunk_count: total as i64,
        },
        rebuilt,
    })
}

/// Список документов проекта (project_id=None — вне проектов), для интерфейса.
#[tauri::command]
pub(crate) fn list_documents(
    app: tauri::AppHandle,
    project_id: Option<String>,
) -> Result<Vec<docstore::DocumentMeta>, String> {
    let conn = docstore::open(&docstore::db_path(&app)?)?;
    docstore::list_documents(&conn, project_id.as_deref())
}

/// Удаление документа из базы (вместе с фрагментами и векторами).
#[tauri::command]
pub(crate) fn delete_document(app: tauri::AppHandle, id: i64) -> Result<(), String> {
    let conn = docstore::open(&docstore::db_path(&app)?)?;
    docstore::delete_document(&conn, id)
}

/// Пуста ли база документов проекта (фронт решает, включать ли поиск перед вопросом).
#[tauri::command]
pub(crate) fn documents_empty(
    app: tauri::AppHandle,
    project_id: Option<String>,
) -> Result<bool, String> {
    let conn = docstore::open(&docstore::db_path(&app)?)?;
    docstore::is_empty(&conn, project_id.as_deref())
}

/// Семантический поиск по базе: эмбеддинг вопроса → KNN top-k фрагментов с
/// метаданными. Пустой запрос/пустая база → пусто (без обращения к эмбеддингам).
#[tauri::command]
pub(crate) async fn search_documents(
    app: tauri::AppHandle,
    query: String,
    k: usize,
    project_id: Option<String>,
) -> AppResult<Vec<docstore::RetrievedChunk>> {
    let q = query.trim();
    if q.is_empty() {
        return Ok(Vec::new());
    }
    // SQLite — блокирующий ввод-вывод, а поиск зовётся перед каждым вопросом:
    // работу с базой уводим в пул блокирующих потоков, соединение через await не держим.
    let db = docstore::db_path(&app)?;
    // пустая база проекта → не трогаем эмбеддинги
    let empty = {
        let db = db.clone();
        let pid = project_id.clone();
        tauri::async_runtime::spawn_blocking(move || {
            let conn = docstore::open(&db)?;
            docstore::is_empty(&conn, pid.as_deref())
        })
        .await
        .map_err(|e| format!("Сбой поиска по базе: {e}"))??
    };
    if empty {
        return Ok(Vec::new());
    }
    let qvec = embed::embed_one(q).await?;
    // Сама база — не движок: её сбои остаются без кода причины (Unknown), и
    // сообщение показывается как есть.
    tauri::async_runtime::spawn_blocking(move || {
        let conn = docstore::open(&db)?;
        docstore::search(&conn, &qvec, k, project_id.as_deref())
    })
    .await
    .map_err(|e| AppError::unknown(format!("Сбой поиска по базе: {e}")))?
    .map_err(AppError::from)
}

#[cfg(test)]
mod tests {
    use super::normalize_attached_image;

    // 64×64 красный квадрат в WebP (сгенерирован cwebp) — фикстура конвертации.
    const WEBP_B64: &str = "UklGRlAAAABXRUJQVlA4IEQAAADQAwCdASpAAEAAPpFIoEwlpCMiIggAsBIJaQB2AAAjPuOsAFeIUwAA/u5DH/6+AKeqP//pNn/BJ/8En8D39HhUCAAAAA==";

    // WebP движок Ollama не понимает — normalize обязан вернуть JPEG/PNG,
    // который снова декодируется (то есть движку он точно по зубам).
    #[test]
    fn webp_is_recoded_to_supported_format() {
        use base64::Engine;
        let webp = base64::engine::general_purpose::STANDARD.decode(WEBP_B64).unwrap();
        let out = normalize_attached_image(webp, "webp").expect("конвертация webp");
        assert!(!out.starts_with(b"RIFF"), "webp должен быть перекодирован");
        assert!(
            out.starts_with(&[0xFF, 0xD8]) || out.starts_with(b"\x89PNG"),
            "ожидался JPEG или PNG, первые байты: {:?}",
            &out[..4.min(out.len())]
        );
        image::load_from_memory(&out).expect("результат снова читается");
    }

    // PNG/JPEG/GIF движок читает сам — байты не трогаем (быстро, без потерь).
    #[test]
    fn non_webp_passes_through_untouched() {
        let bytes = vec![1u8, 2, 3];
        assert_eq!(normalize_attached_image(bytes.clone(), "png").unwrap(), bytes);
        assert_eq!(normalize_attached_image(bytes.clone(), "gif").unwrap(), bytes);
    }

    #[test]
    fn corrupt_webp_gives_friendly_error() {
        let err = normalize_attached_image(vec![0u8; 10], "webp").unwrap_err();
        assert!(err.contains("WEBP"), "{err}");
    }
}
