// ── Проекты: группировка чатов + своя база знаний ───────────────────────────

use serde::{Deserialize, Serialize};
use tauri::Manager;

use crate::docstore;
use crate::history::{conversations_dir, Conversation};
use crate::{validate_id, write_atomic};

/// То же, что SettingsLock для settings.json, только для projects.json:
/// save_project/delete_project делают read-modify-write всего файла, а Tauri
/// выполняет sync-команды параллельно — без лока две близкие операции
/// (переименование + правка инструкций) затирали бы правки друг друга.
pub(crate) struct ProjectsLock(pub(crate) std::sync::Mutex<()>);

// Проекты хранятся одним файлом appDataDir/projects.json (массив). Диалоги и
// документы ссылаются на project_id. Модель как в Claude: у проекта свои чаты и
// своя база документов; быстрые чаты и общие документы живут вне проектов (None).

/// Проект: имя + опциональные инструкции (системная подсказка для чатов проекта).
#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct Project {
    id: String,
    name: String,
    #[serde(default)]
    instructions: String,
    created_at: i64,
    updated_at: i64,
}

fn projects_path(app: &tauri::AppHandle) -> Result<std::path::PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Не удалось получить appDataDir: {e}"))?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("Не удалось создать каталог: {e}"))?;
    Ok(dir.join("projects.json"))
}

fn load_projects(app: &tauri::AppHandle) -> Result<Vec<Project>, String> {
    let path = projects_path(app)?;
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return Ok(Vec::new()), // файла нет — проектов ещё нет
    };
    Ok(serde_json::from_str::<Vec<Project>>(&text).unwrap_or_default())
}

fn save_projects(app: &tauri::AppHandle, projects: &[Project]) -> Result<(), String> {
    let path = projects_path(app)?;
    let text = serde_json::to_string_pretty(projects).map_err(|e| e.to_string())?;
    write_atomic(&path, &text)
}

/// Список проектов (свежие по обновлению сверху).
#[tauri::command]
pub(crate) fn list_projects(app: tauri::AppHandle) -> Result<Vec<Project>, String> {
    let mut projects = load_projects(&app)?;
    projects.sort_by_key(|p| std::cmp::Reverse(p.updated_at));
    Ok(projects)
}

/// Создать/обновить проект (upsert по id). Фронт задаёт id (UUID), имя, инструкции.
/// Одна точка для создания, переименования и правки инструкций.
#[tauri::command]
pub(crate) fn save_project(
    app: tauri::AppHandle,
    lock: tauri::State<'_, ProjectsLock>,
    project: Project,
) -> Result<(), String> {
    validate_id(&project.id)?;
    let _guard = lock.0.lock().unwrap_or_else(|e| e.into_inner());
    let mut projects = load_projects(&app)?;
    if let Some(existing) = projects.iter_mut().find(|p| p.id == project.id) {
        existing.name = project.name;
        existing.instructions = project.instructions;
        existing.updated_at = project.updated_at;
    } else {
        projects.push(project);
    }
    save_projects(&app, &projects)
}

/// Удалить проект вместе с его чатами и документами (каскад). Необратимо.
#[tauri::command]
pub(crate) fn delete_project(
    app: tauri::AppHandle,
    lock: tauri::State<'_, ProjectsLock>,
    id: String,
) -> Result<(), String> {
    validate_id(&id)?;
    let _guard = lock.0.lock().unwrap_or_else(|e| e.into_inner());

    // 1) сначала документы проекта (фрагменты + векторы). Если чистка базы не
    // удалась (занята, не открылась) — проект ОСТАЁТСЯ в списке и удаление можно
    // повторить. Иначе тексты документов повисали бы в базе без владельца, а
    // пользователь считал бы их удалёнными («удалил — значит удалено», 152-ФЗ).
    let conn = docstore::open(&docstore::db_path(&app)?)
        .map_err(|e| format!("Проект не удалён — база документов недоступна: {e}"))?;
    let doc_ids = docstore::document_ids_for_project(&conn, &id)
        .map_err(|e| format!("Проект не удалён — не удалось прочитать его документы: {e}"))?;
    let failed: Vec<String> = doc_ids
        .into_iter()
        .filter_map(|doc_id| docstore::delete_document(&conn, doc_id).err())
        .collect();
    if !failed.is_empty() {
        return Err(format!(
            "Проект не удалён: не удалось вычистить его документы из базы ({}). \
             Повторите удаление.",
            failed.join("; ")
        ));
    }

    // 2) удалить чаты этого проекта (файлы, где project_id == id)
    if let Ok(dir) = conversations_dir(&app) {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                if let Ok(text) = std::fs::read_to_string(&path) {
                    if let Ok(conv) = serde_json::from_str::<Conversation>(&text) {
                        if conv.project_id.as_deref() == Some(id.as_str()) {
                            let _ = std::fs::remove_file(&path);
                        }
                    }
                }
            }
        }
    }

    // 3) убрать проект из списка — только после успешной чистки его данных
    let projects: Vec<Project> = load_projects(&app)?
        .into_iter()
        .filter(|p| p.id != id)
        .collect();
    save_projects(&app, &projects)
}
