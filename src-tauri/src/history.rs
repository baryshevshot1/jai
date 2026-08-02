// ── История диалогов: хранение в appDataDir/conversations/<id>.json ─────────
// Один файл на диалог: список в панели строится по мета-шапкам (с кэшем), полное
// содержимое читается только при открытии.

use serde::{Deserialize, Serialize};
use std::sync::LazyLock;
use tauri::Manager;

use crate::chat::ChatMessage;
use crate::{validate_id, write_atomic};

/// Полный диалог (как лежит в файле). `project_id` — принадлежность проекту
/// (None/отсутствует = быстрый чат вне проектов; старые файлы читаются как None).
#[derive(Serialize, Deserialize)]
pub(crate) struct Conversation {
    id: String,
    title: String,
    updated_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) project_id: Option<String>,
    messages: Vec<ChatMessage>,
}

/// Краткая карточка диалога для списка в боковой панели.
#[derive(Serialize, Clone)]
pub(crate) struct ConversationMeta {
    id: String,
    title: String,
    updated_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    project_id: Option<String>,
}

/// Шапка файла диалога — только мета-поля для списка. messages при разборе
/// пропускаются: в vision-чатах там мегабайты base64-картинок, и строить их в
/// память ради четырёх полей незачем.
#[derive(Deserialize)]
struct ConversationHead {
    id: String,
    title: String,
    updated_at: i64,
    #[serde(default)]
    project_id: Option<String>,
}

/// Кэш карточек для list_conversations: путь → (mtime, размер, карточка). Список
/// зовётся после КАЖДОГО ответа в чате — без кэша каждый ход перечитывал бы с диска
/// все диалоги целиком. Перепарсиваются только файлы с изменившимися mtime+размером.
#[allow(clippy::type_complexity)]
static CONV_META_CACHE: LazyLock<
    std::sync::Mutex<
        std::collections::HashMap<std::path::PathBuf, (std::time::SystemTime, u64, ConversationMeta)>,
    >,
> = LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// Каталог с диалогами внутри appDataDir (кроссплатформенно). Создаём при необходимости.
pub(crate) fn conversations_dir(app: &tauri::AppHandle) -> Result<std::path::PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Не удалось получить appDataDir: {e}"))?
        .join("conversations");
    std::fs::create_dir_all(&dir).map_err(|e| format!("Не удалось создать каталог: {e}"))?;
    Ok(dir)
}

#[tauri::command]
pub(crate) fn list_conversations(app: tauri::AppHandle) -> Result<Vec<ConversationMeta>, String> {
    let dir = conversations_dir(&app)?;
    let mut metas: Vec<ConversationMeta> = Vec::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Ok(metas), // каталога нет/пуст — пустой список, не ошибка
    };
    let mut cache = CONV_META_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    // Кэш пересобирается из увиденных файлов: удалённые диалоги выпадают сами.
    let mut fresh = std::collections::HashMap::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(fs_meta) = entry.metadata() else { continue };
        let stamp = (
            fs_meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH),
            fs_meta.len(),
        );
        // Файл не менялся с прошлого списка — карточка из кэша, без чтения с диска.
        if let Some((t, len, meta)) = cache.get(&path) {
            if (*t, *len) == stamp {
                metas.push(meta.clone());
                fresh.insert(path, (stamp.0, stamp.1, meta.clone()));
                continue;
            }
        }
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(head) = serde_json::from_str::<ConversationHead>(&text) {
                let meta = ConversationMeta {
                    id: head.id,
                    title: head.title,
                    updated_at: head.updated_at,
                    project_id: head.project_id,
                };
                metas.push(meta.clone());
                fresh.insert(path, (stamp.0, stamp.1, meta));
            }
        }
    }
    *cache = fresh;
    metas.sort_by_key(|m| std::cmp::Reverse(m.updated_at)); // свежие сверху
    Ok(metas)
}

#[tauri::command]
pub(crate) fn load_conversation(app: tauri::AppHandle, id: String) -> Result<Conversation, String> {
    validate_id(&id)?;
    let path = conversations_dir(&app)?.join(format!("{id}.json"));
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("Не удалось прочитать диалог: {e}"))?;
    serde_json::from_str::<Conversation>(&text)
        .map_err(|e| format!("Повреждённый файл диалога: {e}"))
}

#[tauri::command]
pub(crate) fn save_conversation(
    app: tauri::AppHandle,
    conversation: Conversation,
) -> Result<(), String> {
    validate_id(&conversation.id)?;
    let path = conversations_dir(&app)?.join(format!("{}.json", conversation.id));
    let text = serde_json::to_string_pretty(&conversation).map_err(|e| e.to_string())?;
    write_atomic(&path, &text)
}

#[tauri::command]
pub(crate) fn delete_conversation(app: tauri::AppHandle, id: String) -> Result<(), String> {
    validate_id(&id)?;
    let path = conversations_dir(&app)?.join(format!("{id}.json"));
    if path.exists() {
        std::fs::remove_file(&path).map_err(|e| format!("Не удалось удалить диалог: {e}"))?;
    }
    Ok(())
}

/// Очистка всех диалогов: удаляем все *.json в каталоге conversations.
#[tauri::command]
pub(crate) fn clear_conversations(app: tauri::AppHandle) -> Result<(), String> {
    let dir = conversations_dir(&app)?;
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    Ok(())
}
