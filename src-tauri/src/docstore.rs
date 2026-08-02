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
    // Указатель на инициализатор расширения приводим к сигнатуре, которую ждёт
    // sqlite3_auto_extension: типы описаны в разных крейтах (libsqlite3-sys и
    // sqlite-vec) и не совпадают номинально, хотя ABI один и тот же.
    type AutoExtension = unsafe extern "C" fn(
        *mut rusqlite::ffi::sqlite3,
        *mut *mut std::os::raw::c_char,
        *const rusqlite::ffi::sqlite3_api_routines,
    ) -> std::os::raw::c_int;
    unsafe {
        let init: AutoExtension = std::mem::transmute(sqlite3_vec_init as *const ());
        sqlite3_auto_extension(Some(init));
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
/// Повреждённый файл (сбой питания, битый диск) НЕ приговор: индекс — кэш поверх
/// исходных файлов пользователя, поэтому убираем повреждённую базу в резервную
/// копию и создаём чистую — приложение продолжает работать, диагностика предупредит.
/// Векторная таблица создаётся отдельно (`ensure_vec_table`), когда известна
/// размерность эмбеддинга.
pub fn open(path: &Path) -> Result<Connection, String> {
    match try_open(path) {
        Ok(conn) => Ok(conn),
        Err(e) if is_corruption(&e) => {
            eprintln!(
                "[jai] база документов повреждена ({e}) — пересоздаю; копия: {}",
                corrupt_backup_path(path).display()
            );
            backup_corrupt(path);
            try_open(path).map_err(|e| format!("Не удалось пересоздать базу: {e}"))
        }
        Err(e) => Err(format!("Не удалось открыть базу: {e}")),
    }
}

/// Открыть + busy_timeout + схема, с ТИПИЗИРОВАННОЙ ошибкой (для классификации
/// повреждения). Владеет соединением: при Err оно уже закрыто — важно для Windows,
/// где переименовать файл с открытым хендлом нельзя.
fn try_open(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    // Каждая команда открывает своё соединение. Без busy_timeout параллельная работа
    // (индексация документа + поиск во время чата) даёт «database is locked». Даём
    // писателю до 5 с дождаться освобождения вместо мгновенной ошибки.
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    init(&conn)?;
    Ok(conn)
}

/// Повреждение файла базы по кодам SQLite. Строго ТОЛЬКО коды порчи: busy/права
/// доступа/нет каталога повреждением не считаются — иначе можно снести живую базу.
fn is_corruption(e: &rusqlite::Error) -> bool {
    matches!(e, rusqlite::Error::SqliteFailure(f, _)
        if matches!(f.code, rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase))
}

/// Путь резервной копии повреждённой базы (рядом с оригиналом). Публичный: диагностика
/// по наличию файла показывает «база пересоздавалась после повреждения».
pub fn corrupt_backup_path(db: &Path) -> PathBuf {
    let mut name = db.as_os_str().to_os_string();
    name.push(".corrupt.bak");
    PathBuf::from(name)
}

/// Убрать повреждённый файл с дороги: db → db.corrupt.bak (старую копию перезаписываем;
/// на Windows rename поверх существующего не работает — сначала удаляем цель).
/// Сайдкары WAL/SHM без своей базы бессмысленны и «отравили» бы новую (свежая база
/// подхватила бы чужой -wal) — удаляем. Ошибки гасим: это уборка, не критичный путь.
fn backup_corrupt(path: &Path) {
    let backup = corrupt_backup_path(path);
    let _ = std::fs::remove_file(&backup);
    let _ = std::fs::rename(path, &backup);
    for suffix in ["-wal", "-shm"] {
        let mut side = path.as_os_str().to_os_string();
        side.push(suffix);
        let _ = std::fs::remove_file(PathBuf::from(side));
    }
}

/// Базовая схема + контроль версии. При несовпадении версии — чистое пересоздание.
/// Ошибка типизированная (rusqlite): open() по ней отличает повреждение файла.
fn init(conn: &Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;

    let version: i64 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap_or(0);

    // База уже есть, но версия другая — индекс несовместим, пересоздаём с нуля.
    if version != 0 && version != SCHEMA_VERSION {
        drop_all(conn)?;
    }

    // Аддитивная миграция ДО создания таблиц: базы, созданные до появления проектов,
    // имеют documents с глобальным UNIQUE(sha256) без project_id. Перестраиваем с
    // сохранением данных (project_id=NULL = вне проектов). При сбое — сброс индекса
    // (кэш поверх исходных файлов), после чего CREATE ниже создаст схему заново.
    migrate_documents_project_id(conn);

    conn.execute_batch(
        // documents.project_id: NULL = документ вне проектов (общая база для быстрых
        // чатов). Уникальность sha256 — В ПРЕДЕЛАХ проекта: один файл можно держать в
        // разных проектах независимо (как в Claude).
        "CREATE TABLE IF NOT EXISTS meta (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS documents (
            id          INTEGER PRIMARY KEY,
            filename    TEXT NOT NULL,
            ext         TEXT NOT NULL,
            sha256      TEXT NOT NULL,
            added_at    INTEGER NOT NULL,
            char_count  INTEGER NOT NULL,
            chunk_count INTEGER NOT NULL,
            project_id  TEXT,
            UNIQUE(project_id, sha256)
        );
        CREATE TABLE IF NOT EXISTS chunks (
            id          INTEGER PRIMARY KEY,
            doc_id      INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
            chunk_index INTEGER NOT NULL,
            text        TEXT NOT NULL,
            page        INTEGER
        );
        CREATE INDEX IF NOT EXISTS idx_chunks_doc ON chunks(doc_id);",
    )?;

    // Уборка сирот от прежних оборванных миграций (до того как перестройка стала
    // транзакционной): фрагменты без документа съедают KNN-пул кандидатов и место.
    // Сначала дешёвая проверка на чтение — в здоровой базе писать нечего.
    let orphans: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM chunks WHERE doc_id NOT IN (SELECT id FROM documents))",
            [],
            |r| r.get(0),
        )
        .unwrap_or(false);
    if orphans {
        // векторы — до фрагментов (тот же порядок, что в delete_document); vec_chunks
        // может ещё не существовать — она создаётся при первой индексации
        let has_vec = conn
            .prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name='vec_chunks'")
            .and_then(|mut s| s.exists([]))
            .unwrap_or(false);
        if has_vec {
            let _ = conn.execute(
                "DELETE FROM vec_chunks WHERE rowid IN
                    (SELECT id FROM chunks WHERE doc_id NOT IN (SELECT id FROM documents))",
                [],
            );
        }
        let _ = conn.execute(
            "DELETE FROM chunks WHERE doc_id NOT IN (SELECT id FROM documents)",
            [],
        );
    }

    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
}

/// Миграция старой схемы documents (глобальный UNIQUE(sha256), без project_id) на
/// новую (project_id + UNIQUE(project_id, sha256)). Данные сохраняем: существующие
/// документы получают project_id=NULL (вне проектов). Если таблицы ещё нет (свежая
/// база) или столбец уже есть — ничего не делаем. При ошибке перестройки сбрасываем
/// индекс (кэш), чтобы приложение продолжило работу с чистой новой схемой.
fn migrate_documents_project_id(conn: &Connection) {
    let table_exists = |name: &str| {
        conn.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1")
            .and_then(|mut s| s.exists([name]))
            .unwrap_or(false)
    };
    // Обрыв дотранзакционной перестройки (крах между DROP и RENAME) мог оставить
    // данные в documents_new без documents — данные целы, довершаем переименование.
    if !table_exists("documents") && table_exists("documents_new") {
        let _ = conn.execute("ALTER TABLE documents_new RENAME TO documents", []);
    }
    if !table_exists("documents") {
        return; // свежая база — мигрировать нечего, CREATE создаст новую схему
    }
    let has_col = conn
        .prepare("SELECT 1 FROM pragma_table_info('documents') WHERE name = 'project_id'")
        .and_then(|mut s| s.exists([]))
        .unwrap_or(true); // при сомнении не трогаем существующую базу
    if has_col {
        return; // уже новая схема
    }

    // Перестройка с переносом данных. Внешние ключи на время миграции выключаем,
    // чтобы DROP/RENAME не спорил с ссылкой chunks.doc_id → documents(id) (id сохраняем).
    // PRAGMA — снаружи (внутри транзакции foreign_keys игнорируется), сами DDL+INSERT —
    // одной транзакцией: сбой посреди (питание, крах) откатывает всё целиком, а не
    // оставляет базу без documents с висячими фрагментами и векторами.
    let _ = conn.pragma_update(None, "foreign_keys", "OFF");
    let rebuilt: rusqlite::Result<()> = conn.execute_batch(
        "BEGIN;
         DROP TABLE IF EXISTS documents_new;
         CREATE TABLE documents_new (
            id          INTEGER PRIMARY KEY,
            filename    TEXT NOT NULL,
            ext         TEXT NOT NULL,
            sha256      TEXT NOT NULL,
            added_at    INTEGER NOT NULL,
            char_count  INTEGER NOT NULL,
            chunk_count INTEGER NOT NULL,
            project_id  TEXT,
            UNIQUE(project_id, sha256)
         );
         INSERT INTO documents_new (id, filename, ext, sha256, added_at, char_count, chunk_count, project_id)
            SELECT id, filename, ext, sha256, added_at, char_count, chunk_count, NULL FROM documents;
         DROP TABLE documents;
         ALTER TABLE documents_new RENAME TO documents;
         COMMIT;",
    );
    if rebuilt.is_err() {
        let _ = conn.execute_batch("ROLLBACK;"); // батч мог упасть внутри открытой транзакции
        let _ = drop_all(conn); // не смогли мигрировать — сбрасываем индекс (пересоберётся)
    }
    let _ = conn.pragma_update(None, "foreign_keys", "ON");
}

/// Полный сброс схемы (при смене версии или несовместимой размерности вектора).
fn drop_all(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "DROP TABLE IF EXISTS vec_chunks;
         DROP TABLE IF EXISTS chunks;
         DROP TABLE IF EXISTS documents;
         DROP TABLE IF EXISTS meta;",
    )
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
            drop_all(conn).map_err(|e| format!("Не удалось пересоздать базу: {e}"))?;
            init(conn).map_err(|e| format!("Не удалось пересоздать базу: {e}"))?;
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

/// Документ с таким sha256 уже в ЭТОМ проекте? (идемпотентность в пределах проекта —
/// один файл можно держать в разных проектах, повторно в одном — не индексируем).
pub fn find_by_hash(
    conn: &Connection,
    sha256: &str,
    project_id: Option<&str>,
) -> Result<Option<DocumentMeta>, String> {
    conn.query_row(
        &format!("SELECT {DOC_COLUMNS} FROM documents WHERE sha256 = ?1 AND project_id IS ?2"),
        rusqlite::params![sha256, project_id],
        row_to_meta,
    )
    .map(Some)
    .or_else(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => Ok(None),
        other => Err(format!("Ошибка поиска документа: {other}")),
    })
}

/// Вставка записи о документе в проект (project_id=None — вне проектов), возвращает id.
// Аргументы — ровно колонки таблицы documents: промежуточная структура здесь только
// добавила бы слой перекладывания полей между вызовом и SQL.
#[allow(clippy::too_many_arguments)]
pub fn insert_document(
    conn: &Connection,
    filename: &str,
    ext: &str,
    sha256: &str,
    added_at: i64,
    char_count: i64,
    chunk_count: i64,
    project_id: Option<&str>,
) -> Result<i64, String> {
    conn.execute(
        "INSERT INTO documents(filename, ext, sha256, added_at, char_count, chunk_count, project_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![filename, ext, sha256, added_at, char_count, chunk_count, project_id],
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

/// Список документов проекта (project_id=None — вне проектов), свежие сверху.
pub fn list_documents(
    conn: &Connection,
    project_id: Option<&str>,
) -> Result<Vec<DocumentMeta>, String> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {DOC_COLUMNS} FROM documents WHERE project_id IS ?1 ORDER BY added_at DESC"
        ))
        .map_err(|e| format!("Ошибка чтения списка документов: {e}"))?;
    let rows = stmt
        .query_map(rusqlite::params![project_id], row_to_meta)
        .map_err(|e| format!("Ошибка чтения списка документов: {e}"))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| format!("Ошибка чтения документа: {e}"))?);
    }
    Ok(out)
}

/// Все документы базы независимо от проекта (для диагностики: общий счёт/размер).
pub fn list_all_documents(conn: &Connection) -> Result<Vec<DocumentMeta>, String> {
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

/// id всех документов проекта (для каскадного удаления вместе с проектом).
pub fn document_ids_for_project(
    conn: &Connection,
    project_id: &str,
) -> Result<Vec<i64>, String> {
    let mut stmt = conn
        .prepare("SELECT id FROM documents WHERE project_id = ?1")
        .map_err(|e| format!("Ошибка чтения документов проекта: {e}"))?;
    let rows = stmt
        .query_map([project_id], |r| r.get::<_, i64>(0))
        .map_err(|e| format!("Ошибка чтения документов проекта: {e}"))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| format!("Ошибка чтения id документа: {e}"))?);
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

/// KNN top-k к вектору запроса в пределах ПРОЕКТА (project_id=None — вне проектов),
/// с join к метаданным. vec0 KNN отдаёт k ближайших ГЛОБАЛЬНО, поэтому фильтр по
/// проекту мог бы обнулить результат — берём расширенный пул кандидатов (k×5) и
/// после фильтра по проекту оставляем top-k.
/// Текст конкретных фрагментов документа по их номерам. Нужен, чтобы показать
/// пользователю, на что именно опирался ответ: это ЧТЕНИЕ по ключу, а не поиск.
/// Через семантический поиск то же самое обошлось бы вычислением эмбеддинга вопроса
/// и загрузкой модели поиска — секунды и лишняя память ради текста, который уже
/// лежит в базе. Вдобавок поиск не гарантирует, что вернёт именно эти фрагменты.
pub fn fragments_by_index(
    conn: &Connection,
    filename: &str,
    chunk_indexes: &[i64],
    project_id: Option<&str>,
) -> Result<Vec<(i64, String)>, String> {
    if chunk_indexes.is_empty() {
        return Ok(Vec::new());
    }
    // Список номеров подставляем плейсхолдерами, а не склейкой строк: номера приходят
    // из интерфейса, и склейка была бы дырой для инъекции.
    let holes = std::iter::repeat_n("?", chunk_indexes.len()).collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT c.chunk_index, c.text
         FROM chunks c JOIN documents d ON d.id = c.doc_id
         WHERE d.filename = ?1 AND d.project_id IS ?2 AND c.chunk_index IN ({holes})
         ORDER BY c.chunk_index"
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("Не удалось подготовить выборку фрагментов: {e}"))?;

    let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![
        Box::new(filename.to_string()),
        Box::new(project_id.map(str::to_string)),
    ];
    for i in chunk_indexes {
        params.push(Box::new(*i));
    }
    let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();

    let rows = stmt
        .query_map(refs.as_slice(), |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
        .map_err(|e| format!("Ошибка выборки фрагментов: {e}"))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| format!("Ошибка чтения фрагмента: {e}"))?);
    }
    Ok(out)
}

pub fn search(
    conn: &Connection,
    query: &[f32],
    k: usize,
    project_id: Option<&str>,
) -> Result<Vec<RetrievedChunk>, String> {
    let pool = (k * 5).max(k) as i64; // пул кандидатов до фильтра по проекту
    let mut stmt = conn
        .prepare(
            "SELECT c.text, d.filename, c.chunk_index, c.page, v.distance
             FROM vec_chunks v
             JOIN chunks c ON c.id = v.rowid
             JOIN documents d ON d.id = c.doc_id
             WHERE v.embedding MATCH ?1 AND k = ?2 AND d.project_id IS ?3
             ORDER BY v.distance LIMIT ?4",
        )
        .map_err(|e| format!("Не удалось подготовить поиск: {e}"))?;
    let rows = stmt
        .query_map(
            rusqlite::params![vec_to_blob(query), pool, project_id, k as i64],
            |r| {
                Ok(RetrievedChunk {
                    text: r.get(0)?,
                    filename: r.get(1)?,
                    chunk_index: r.get(2)?,
                    page: r.get(3)?,
                    distance: r.get(4)?,
                })
            },
        )
        .map_err(|e| format!("Ошибка поиска: {e}"))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| format!("Ошибка чтения результата: {e}"))?);
    }
    Ok(out)
}

/// Пуста ли база проекта (project_id=None — вне проектов): включать ли поиск.
pub fn is_empty(conn: &Connection, project_id: Option<&str>) -> Result<bool, String> {
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM documents WHERE project_id IS ?1",
            rusqlite::params![project_id],
            |r| r.get(0),
        )
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
            let id = insert_document(&conn, name, "txt", &format!("hash{i}"), i as i64, text.chars().count() as i64, 1, None).unwrap();
            let cid = insert_chunk(&conn, id, 0, text, None).unwrap();
            insert_vector(&conn, cid, v).unwrap();
        }
        assert_eq!(list_documents(&conn, None).unwrap().len(), 3);

        // вопрос про стоимость аренды → ближайший фрагмент про арендную плату
        let q = crate::embed::embed_one("сколько стоит аренда помещения в месяц").await.unwrap();
        let hits = knn(&conn, &q, 3).unwrap();
        let top: String = conn
            .query_row("SELECT text FROM chunks WHERE id=?1", [hits[0].0], |r| r.get(0))
            .unwrap();
        eprintln!("top-1: {top}");
        assert!(top.contains("50000"), "релевантный фрагмент про арендную плату должен быть сверху");

        // путь search(): тот же top, но уже с метаданными источника
        let found = search(&conn, &q, 3, None).unwrap();
        assert_eq!(found[0].filename, "аренда.txt", "источник top-1 — документ про аренду");
        assert!(found[0].text.contains("50000"));

        // удаление чистит и фрагменты, и векторы
        let first = list_documents(&conn, None).unwrap();
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

    // Изоляция по проектам: документы одного проекта не видны в другом и вне проектов.
    #[test]
    fn per_project_isolation() {
        register_vec();
        let conn = Connection::open_in_memory().unwrap();
        init(&conn).unwrap();
        ensure_vec_table(&conn, 4).unwrap(); // как после первой индексации

        // один и тот же sha в разных проектах допустим (UNIQUE(project_id, sha256))
        insert_document(&conn, "a.txt", "txt", "sha1", 1, 10, 1, Some("projA")).unwrap();
        insert_document(&conn, "a.txt", "txt", "sha1", 2, 10, 1, Some("projB")).unwrap();
        insert_document(&conn, "g.txt", "txt", "sha2", 3, 10, 1, None).unwrap();

        assert_eq!(list_documents(&conn, Some("projA")).unwrap().len(), 1);
        assert_eq!(list_documents(&conn, Some("projB")).unwrap().len(), 1);
        assert_eq!(list_documents(&conn, None).unwrap().len(), 1, "вне проектов — свой один");
        assert_eq!(list_all_documents(&conn).unwrap().len(), 3, "всего три записи");

        assert!(!is_empty(&conn, Some("projA")).unwrap());
        assert!(is_empty(&conn, Some("projC")).unwrap(), "в пустом проекте — пусто");

        // дубль в пределах одного проекта отсекается find_by_hash
        assert!(find_by_hash(&conn, "sha1", Some("projA")).unwrap().is_some());
        assert!(find_by_hash(&conn, "sha1", Some("projC")).unwrap().is_none());
        assert!(find_by_hash(&conn, "sha2", None).unwrap().is_some());

        // каскад по проекту: id документов projA → удаление
        let ids = document_ids_for_project(&conn, "projA").unwrap();
        assert_eq!(ids.len(), 1);
        delete_document(&conn, ids[0]).unwrap();
        assert!(is_empty(&conn, Some("projA")).unwrap(), "после удаления projA пуст");
        assert_eq!(list_all_documents(&conn).unwrap().len(), 2);
    }

    // Миграция старой схемы (глобальный UNIQUE(sha256), без project_id): данные
    // сохраняются, столбец project_id появляется со значением NULL (вне проектов).
    #[test]
    fn migrates_old_documents_schema() {
        let conn = Connection::open_in_memory().unwrap();
        // воспроизводим СТАРУЮ таблицу до появления проектов
        conn.execute_batch(
            "CREATE TABLE documents (
                id INTEGER PRIMARY KEY, filename TEXT NOT NULL, ext TEXT NOT NULL,
                sha256 TEXT NOT NULL UNIQUE, added_at INTEGER NOT NULL,
                char_count INTEGER NOT NULL, chunk_count INTEGER NOT NULL
            );
            INSERT INTO documents(filename, ext, sha256, added_at, char_count, chunk_count)
                VALUES ('old.txt','txt','oldsha',1,10,1);",
        )
        .unwrap();

        // init() запускает миграцию: столбец добавлен, запись на месте, project_id=NULL
        init(&conn).unwrap();
        let has_col: bool = conn
            .prepare("SELECT 1 FROM pragma_table_info('documents') WHERE name='project_id'")
            .and_then(|mut s| s.exists([]))
            .unwrap();
        assert!(has_col, "после миграции есть столбец project_id");
        let docs = list_documents(&conn, None).unwrap();
        assert_eq!(docs.len(), 1, "старый документ сохранён и относится к «вне проектов»");
        assert_eq!(docs[0].filename, "old.txt");
    }

    // Обрыв старой (дотранзакционной) миграции между DROP и RENAME: documents нет,
    // данные лежат в documents_new. init() довершает переименование — данные целы.
    #[test]
    fn completes_interrupted_migration() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE documents_new (
                id          INTEGER PRIMARY KEY,
                filename    TEXT NOT NULL,
                ext         TEXT NOT NULL,
                sha256      TEXT NOT NULL,
                added_at    INTEGER NOT NULL,
                char_count  INTEGER NOT NULL,
                chunk_count INTEGER NOT NULL,
                project_id  TEXT,
                UNIQUE(project_id, sha256)
            );
            INSERT INTO documents_new VALUES (1,'saved.txt','txt','sha',1,10,1,NULL);",
        )
        .unwrap();

        init(&conn).unwrap();
        let docs = list_documents(&conn, None).unwrap();
        assert_eq!(docs.len(), 1, "данные оборванной миграции спасены");
        assert_eq!(docs[0].filename, "saved.txt");
    }

    // Висячие фрагменты и векторы (последствие старого сбоя миграции) вычищаются
    // при открытии; живые данные не трогаются.
    #[test]
    fn cleans_orphan_chunks_and_vectors() {
        register_vec();
        let conn = Connection::open_in_memory().unwrap();
        init(&conn).unwrap();
        ensure_vec_table(&conn, 4).unwrap();
        let id = insert_document(&conn, "a.txt", "txt", "sha", 1, 10, 1, None).unwrap();
        let cid = insert_chunk(&conn, id, 0, "текст", None).unwrap();
        insert_vector(&conn, cid, &[1.0, 0.0, 0.0, 0.0]).unwrap();

        // осиротим: удалим документ в обход каскада (эмуляция последствий сбоя)
        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        conn.execute("DELETE FROM documents", []).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();

        init(&conn).unwrap(); // повторное открытие
        let chunks: i64 = conn.query_row("SELECT count(*) FROM chunks", [], |r| r.get(0)).unwrap();
        let vecs: i64 = conn.query_row("SELECT count(*) FROM vec_chunks", [], |r| r.get(0)).unwrap();
        assert_eq!(chunks, 0, "висячие фрагменты вычищены");
        assert_eq!(vecs, 0, "висячие векторы вычищены");
    }

    // Повреждённый файл базы: open() убирает его в .corrupt.bak и создаёт чистую
    // работоспособную базу (индекс — кэш, приложение не должно ломаться).
    #[test]
    fn recovers_from_corrupt_db() {
        register_vec();
        let dir = std::env::temp_dir().join(format!("jai-corrupt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("documents.db");
        std::fs::write(&db, b"this is definitely not a sqlite database").unwrap();

        let conn = open(&db).expect("повреждённая база должна пересоздаться");
        assert!(
            corrupt_backup_path(&db).exists(),
            "копия повреждённого файла сохранена рядом"
        );
        // новая база работоспособна
        insert_document(&conn, "a.txt", "txt", "sha", 1, 10, 1, None).unwrap();
        assert_eq!(list_documents(&conn, None).unwrap().len(), 1);
        drop(conn);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // НЕ-повреждение (нет каталога → CannotOpen) не должно приводить к пересозданию:
    // честная ошибка, никаких резервных копий и удалений.
    #[test]
    fn no_recovery_on_non_corruption() {
        let db = std::env::temp_dir()
            .join(format!("jai-noexist-{}", std::process::id()))
            .join("nope")
            .join("documents.db");
        assert!(open(&db).is_err(), "путь без каталога — честная ошибка");
        assert!(!corrupt_backup_path(&db).exists(), "резервная копия не создаётся");
    }
}
