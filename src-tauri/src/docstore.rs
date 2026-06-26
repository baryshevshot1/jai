// ── База документов (Фаза B): векторное хранилище на SQLite + sqlite-vec ──────
//
// Отдельный файл БД `appDataDir/documents.db` (рядом с conversations/). Хранит:
//   • documents — добавленные файлы (имя, sha256 для идемпотентности, кол-во фрагментов);
//   • chunks    — текстовые фрагменты документов с метаданными;
//   • vec_chunks — виртуальная таблица vec0 с эмбеддингами (rowid = chunks.id).
// Метрика — косинусная (bge-m3 рассчитан на косинус). Размерность вектора НЕ
// хардкодим: её задаёт первый эмбеддинг (Фаза B2) и фиксирует в meta.
//
// Здесь — фундамент (B1): регистрация расширения, схема, низкоуровневые помощники
// вставки/поиска векторов. Эмбеддинги, чанкинг и команды — на следующих под-шагах.

use rusqlite::{ffi::sqlite3_auto_extension, Connection};
use serde::Serialize;
use sqlite_vec::sqlite3_vec_init;
use std::path::{Path, PathBuf};
use zerocopy::IntoBytes;

/// Версия схемы БД. При несовпадении базу пересоздаём (данные индекса теряются —
/// это кэш поверх исходных файлов, не пользовательская история), не падаем.
const SCHEMA_VERSION: i64 = 1;

/// Регистрирует C-расширение sqlite-vec для ВСЕХ последующих соединений процесса.
/// Вызывать один раз при старте (до открытия любых соединений). Идемпотентно по сути,
/// но дёргать повторно не нужно.
pub fn register_vec() {
    unsafe {
        sqlite3_auto_extension(Some(std::mem::transmute(
            sqlite3_vec_init as *const (),
        )));
    }
}

/// Путь к файлу базы документов внутри appDataDir (кроссплатформенно).
pub fn db_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    use tauri::Manager;
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Не удалось получить appDataDir: {e}"))?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("Не удалось создать каталог: {e}"))?;
    Ok(dir.join("documents.db"))
}

/// Открывает (создаёт при необходимости) базу и гарантирует базовую схему.
/// Векторная таблица создаётся отдельно (`ensure_vec_table`), когда известна
/// размерность эмбеддинга.
pub fn open(path: &Path) -> Result<Connection, String> {
    let conn = Connection::open(path).map_err(|e| format!("Не удалось открыть базу: {e}"))?;
    init(&conn)?;
    Ok(conn)
}

/// Базовая схема + контроль версии. При несовпадении версии — чистое пересоздание.
fn init(conn: &Connection) -> Result<(), String> {
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(|e| format!("Не удалось включить WAL: {e}"))?;
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(|e| format!("Не удалось включить foreign_keys: {e}"))?;

    let version: i64 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap_or(0);

    // База уже есть, но версия другая — индекс несовместим, пересоздаём с нуля.
    if version != 0 && version != SCHEMA_VERSION {
        drop_all(conn)?;
    }

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS meta (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS documents (
            id          INTEGER PRIMARY KEY,
            filename    TEXT NOT NULL,
            ext         TEXT NOT NULL,
            sha256      TEXT NOT NULL UNIQUE,
            added_at    INTEGER NOT NULL,
            char_count  INTEGER NOT NULL,
            chunk_count INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS chunks (
            id          INTEGER PRIMARY KEY,
            doc_id      INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
            chunk_index INTEGER NOT NULL,
            text        TEXT NOT NULL,
            page        INTEGER
        );
        CREATE INDEX IF NOT EXISTS idx_chunks_doc ON chunks(doc_id);",
    )
    .map_err(|e| format!("Не удалось создать схему: {e}"))?;

    conn.pragma_update(None, "user_version", SCHEMA_VERSION)
        .map_err(|e| format!("Не удалось записать версию схемы: {e}"))?;
    Ok(())
}

/// Полный сброс схемы (при смене версии или несовместимой размерности вектора).
fn drop_all(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "DROP TABLE IF EXISTS vec_chunks;
         DROP TABLE IF EXISTS chunks;
         DROP TABLE IF EXISTS documents;
         DROP TABLE IF EXISTS meta;",
    )
    .map_err(|e| format!("Не удалось пересоздать базу: {e}"))
}

/// Гарантирует наличие векторной таблицы нужной размерности. Если размерность в
/// базе отличается от пришедшей (сменили модель эмбеддингов) — пересоздаёт ВСЮ базу
/// и сообщает об этом (true = была пересоздана, индекс нужно построить заново).
pub fn ensure_vec_table(conn: &Connection, dim: usize) -> Result<bool, String> {
    let stored: Option<i64> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'embed_dim'",
            [],
            |r| r.get::<_, String>(0),
        )
        .ok()
        .and_then(|s| s.parse().ok());

    let mut rebuilt = false;
    if let Some(d) = stored {
        if d as usize != dim {
            // Несовместимая размерность — старый индекс непригоден.
            drop_all(conn)?;
            init(conn)?;
            rebuilt = true;
        }
    }

    // vec0 не поддерживает параметризацию размерности через bind — только в DDL,
    // поэтому подставляем проверенное число (dim приходит из длины эмбеддинга).
    conn.execute_batch(&format!(
        "CREATE VIRTUAL TABLE IF NOT EXISTS vec_chunks USING vec0(
            embedding float[{dim}] distance_metric=cosine
        );"
    ))
    .map_err(|e| format!("Не удалось создать векторную таблицу: {e}"))?;

    conn.execute(
        "INSERT OR REPLACE INTO meta(key, value) VALUES ('embed_dim', ?1)",
        [dim.to_string()],
    )
    .map_err(|e| format!("Не удалось записать размерность: {e}"))?;
    conn.execute(
        "INSERT OR REPLACE INTO meta(key, value) VALUES ('metric', 'cosine')",
        [],
    )
    .map_err(|e| format!("Не удалось записать метрику: {e}"))?;
    Ok(rebuilt)
}

/// Эмбеддинг (Vec<f32>) → BLOB из f32 little-endian для хранения в vec0 (без копий).
pub fn vec_to_blob(v: &[f32]) -> &[u8] {
    v.as_bytes()
}

/// Вставка вектора фрагмента: rowid в vec_chunks совпадает с chunks.id.
pub fn insert_vector(conn: &Connection, chunk_id: i64, embedding: &[f32]) -> Result<(), String> {
    conn.execute(
        "INSERT INTO vec_chunks(rowid, embedding) VALUES (?1, ?2)",
        rusqlite::params![chunk_id, vec_to_blob(embedding)],
    )
    .map_err(|e| format!("Не удалось сохранить вектор: {e}"))?;
    Ok(())
}

/// KNN-поиск без метаданных: (chunk_id, distance). Низкоуровневый примитив —
/// в командах используется search() с join; оставлен и покрыт smoke-тестом.
#[allow(dead_code)]
pub fn knn(conn: &Connection, query: &[f32], k: usize) -> Result<Vec<(i64, f32)>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT rowid, distance FROM vec_chunks
             WHERE embedding MATCH ?1 ORDER BY distance LIMIT ?2",
        )
        .map_err(|e| format!("Не удалось подготовить поиск: {e}"))?;
    let rows = stmt
        .query_map(rusqlite::params![vec_to_blob(query), k as i64], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, f32>(1)?))
        })
        .map_err(|e| format!("Ошибка поиска: {e}"))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| format!("Ошибка чтения результата: {e}"))?);
    }
    Ok(out)
}

// ── Документы и фрагменты (B3): запись/чтение/удаление ───────────────────────

/// Карточка документа для интерфейса базы.
#[derive(Serialize, Clone)]
pub struct DocumentMeta {
    pub id: i64,
    pub filename: String,
    pub ext: String,
    pub added_at: i64,
    pub char_count: i64,
    pub chunk_count: i64,
}

const DOC_COLUMNS: &str = "id, filename, ext, added_at, char_count, chunk_count";

fn row_to_meta(r: &rusqlite::Row) -> rusqlite::Result<DocumentMeta> {
    Ok(DocumentMeta {
        id: r.get(0)?,
        filename: r.get(1)?,
        ext: r.get(2)?,
        added_at: r.get(3)?,
        char_count: r.get(4)?,
        chunk_count: r.get(5)?,
    })
}

/// Документ с таким sha256 уже в базе? (идемпотентность — не индексируем повторно).
pub fn find_by_hash(conn: &Connection, sha256: &str) -> Result<Option<DocumentMeta>, String> {
    conn.query_row(
        &format!("SELECT {DOC_COLUMNS} FROM documents WHERE sha256 = ?1"),
        [sha256],
        row_to_meta,
    )
    .map(Some)
    .or_else(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => Ok(None),
        other => Err(format!("Ошибка поиска документа: {other}")),
    })
}

/// Вставка записи о документе, возвращает его id.
pub fn insert_document(
    conn: &Connection,
    filename: &str,
    ext: &str,
    sha256: &str,
    added_at: i64,
    char_count: i64,
    chunk_count: i64,
) -> Result<i64, String> {
    conn.execute(
        "INSERT INTO documents(filename, ext, sha256, added_at, char_count, chunk_count)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![filename, ext, sha256, added_at, char_count, chunk_count],
    )
    .map_err(|e| format!("Не удалось сохранить документ: {e}"))?;
    Ok(conn.last_insert_rowid())
}

/// Вставка фрагмента, возвращает его id (он же rowid для вектора в vec_chunks).
pub fn insert_chunk(
    conn: &Connection,
    doc_id: i64,
    chunk_index: i64,
    text: &str,
    page: Option<i64>,
) -> Result<i64, String> {
    conn.execute(
        "INSERT INTO chunks(doc_id, chunk_index, text, page) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![doc_id, chunk_index, text, page],
    )
    .map_err(|e| format!("Не удалось сохранить фрагмент: {e}"))?;
    Ok(conn.last_insert_rowid())
}

/// Список документов (свежие сверху).
pub fn list_documents(conn: &Connection) -> Result<Vec<DocumentMeta>, String> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {DOC_COLUMNS} FROM documents ORDER BY added_at DESC"
        ))
        .map_err(|e| format!("Ошибка чтения списка документов: {e}"))?;
    let rows = stmt
        .query_map([], row_to_meta)
        .map_err(|e| format!("Ошибка чтения списка документов: {e}"))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| format!("Ошибка чтения документа: {e}"))?);
    }
    Ok(out)
}

/// Удаление документа: чистит фрагменты (CASCADE) И векторы (vec0 каскад не покрывает).
pub fn delete_document(conn: &Connection, id: i64) -> Result<(), String> {
    // векторы — по rowid фрагментов этого документа (до удаления фрагментов)
    conn.execute(
        "DELETE FROM vec_chunks WHERE rowid IN (SELECT id FROM chunks WHERE doc_id = ?1)",
        [id],
    )
    .map_err(|e| format!("Не удалось удалить векторы: {e}"))?;
    conn.execute("DELETE FROM documents WHERE id = ?1", [id])
        .map_err(|e| format!("Не удалось удалить документ: {e}"))?;
    Ok(())
}

// ── Поиск (B4): KNN-фрагменты с метаданными для контекста и источников ───────

/// Найденный фрагмент: текст + происхождение (для контекста модели и показа источников).
#[derive(Serialize, Clone)]
pub struct RetrievedChunk {
    pub text: String,
    pub filename: String,
    pub chunk_index: i64,
    pub page: Option<i64>,
    pub distance: f32,
}

/// KNN top-k к вектору запроса с join к метаданным (имя документа, № фрагмента).
/// Одним запросом: vec0 MATCH + ORDER BY distance + LIMIT, join по rowid=chunks.id.
pub fn search(conn: &Connection, query: &[f32], k: usize) -> Result<Vec<RetrievedChunk>, String> {
    let mut stmt = conn
        .prepare(
            // k=? — KNN-констрейнта vec0 (при join LIMIT не проталкивается в vec0).
            "SELECT c.text, d.filename, c.chunk_index, c.page, v.distance
             FROM vec_chunks v
             JOIN chunks c ON c.id = v.rowid
             JOIN documents d ON d.id = c.doc_id
             WHERE v.embedding MATCH ?1 AND k = ?2 ORDER BY v.distance",
        )
        .map_err(|e| format!("Не удалось подготовить поиск: {e}"))?;
    let rows = stmt
        .query_map(rusqlite::params![vec_to_blob(query), k as i64], |r| {
            Ok(RetrievedChunk {
                text: r.get(0)?,
                filename: r.get(1)?,
                chunk_index: r.get(2)?,
                page: r.get(3)?,
                distance: r.get(4)?,
            })
        })
        .map_err(|e| format!("Ошибка поиска: {e}"))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| format!("Ошибка чтения результата: {e}"))?);
    }
    Ok(out)
}

/// Пуста ли база (для фронта: включать ли поиск по документам).
pub fn is_empty(conn: &Connection) -> Result<bool, String> {
    let n: i64 = conn
        .query_row("SELECT count(*) FROM documents", [], |r| r.get(0))
        .map_err(|e| format!("Ошибка чтения базы: {e}"))?;
    Ok(n == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Smoke-тест B1: регистрация расширения, создание vec0, вставка векторов,
    // KNN с косинусной метрикой и проверка порядка близости.
    #[test]
    fn smoke_vec_store() {
        register_vec();
        let conn = Connection::open_in_memory().unwrap();
        init(&conn).unwrap();

        // версия sqlite-vec доступна → расширение зарегистрировано
        let ver: String = conn
            .query_row("SELECT vec_version()", [], |r| r.get(0))
            .expect("sqlite-vec не зарегистрирован");
        assert!(ver.starts_with('v'), "неожиданная версия vec: {ver}");

        ensure_vec_table(&conn, 4).unwrap();

        insert_vector(&conn, 1, &[1.0, 0.0, 0.0, 0.0]).unwrap();
        insert_vector(&conn, 2, &[0.0, 1.0, 0.0, 0.0]).unwrap();
        insert_vector(&conn, 3, &[0.9, 0.1, 0.0, 0.0]).unwrap();

        let hits = knn(&conn, &[1.0, 0.0, 0.0, 0.0], 2).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].0, 1, "ближайший должен быть точный вектор (rowid=1)");
        assert_eq!(hits[1].0, 3, "второй по близости — почти коллинеарный (rowid=3)");
        assert!(hits[0].1 <= hits[1].1, "дистанции должны возрастать");
    }

    // Живой end-to-end (B3 + превью B4): индексируем три «документа» на разные темы,
    // ищем по смыслу — релевантный должен оказаться сверху; удаление чистит всё.
    // Требует Ollama + bge-m3 → #[ignore]; запуск:
    //   cargo test docstore::tests::index_search_delete_live -- --ignored --nocapture
    #[ignore]
    #[tokio::test]
    async fn index_search_delete_live() {
        register_vec();
        let conn = Connection::open_in_memory().unwrap();
        init(&conn).unwrap();

        let docs = [
            ("аренда.txt", "Арендная плата составляет 50000 рублей в месяц."),
            ("гарантия.txt", "Гарантийный срок на оборудование двенадцать месяцев."),
            ("суд.txt", "Споры разрешаются в арбитражном суде города Москвы."),
        ];
        let texts: Vec<String> = docs.iter().map(|(_, t)| t.to_string()).collect();
        let vecs = crate::embed::embed_batch(&texts).await.expect("эмбеддинг документов");
        ensure_vec_table(&conn, vecs[0].len()).unwrap();

        for (i, ((name, text), v)) in docs.iter().zip(&vecs).enumerate() {
            let id = insert_document(&conn, name, "txt", &format!("hash{i}"), i as i64, text.chars().count() as i64, 1).unwrap();
            let cid = insert_chunk(&conn, id, 0, text, None).unwrap();
            insert_vector(&conn, cid, v).unwrap();
        }
        assert_eq!(list_documents(&conn).unwrap().len(), 3);

        // вопрос про стоимость аренды → ближайший фрагмент про арендную плату
        let q = crate::embed::embed_one("сколько стоит аренда помещения в месяц").await.unwrap();
        let hits = knn(&conn, &q, 3).unwrap();
        let top: String = conn
            .query_row("SELECT text FROM chunks WHERE id=?1", [hits[0].0], |r| r.get(0))
            .unwrap();
        eprintln!("top-1: {top}");
        assert!(top.contains("50000"), "релевантный фрагмент про арендную плату должен быть сверху");

        // путь search(): тот же top, но уже с метаданными источника
        let found = search(&conn, &q, 3).unwrap();
        assert_eq!(found[0].filename, "аренда.txt", "источник top-1 — документ про аренду");
        assert!(found[0].text.contains("50000"));

        // удаление чистит и фрагменты, и векторы
        let first = list_documents(&conn).unwrap();
        let id = first.iter().find(|d| d.filename == "аренда.txt").unwrap().id;
        delete_document(&conn, id).unwrap();
        let nvec: i64 = conn.query_row("SELECT count(*) FROM vec_chunks", [], |r| r.get(0)).unwrap();
        assert_eq!(nvec, 2, "после удаления одного документа остаётся два вектора");
    }

    // Контроль версии схемы: смена размерности вектора пересоздаёт базу.
    #[test]
    fn rebuild_on_dim_change() {
        register_vec();
        let conn = Connection::open_in_memory().unwrap();
        init(&conn).unwrap();
        ensure_vec_table(&conn, 4).unwrap();
        insert_vector(&conn, 1, &[1.0, 0.0, 0.0, 0.0]).unwrap();

        // другая размерность → база пересоздана, флаг true
        let rebuilt = ensure_vec_table(&conn, 8).unwrap();
        assert!(rebuilt, "при смене размерности база должна пересоздаться");
        let n: i64 = conn
            .query_row("SELECT count(*) FROM vec_chunks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "после пересоздания индекс пуст");
    }
}
