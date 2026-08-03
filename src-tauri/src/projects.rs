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
#[derive(Serialize, Deserialize, Clone, Debug)]
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

/// Прочитать список проектов.
///
/// «Проектов нет» и «не смог прочитать» — РАЗНЫЕ вещи, и путать их нельзя. Раньше
/// путались: любой отказ чтения и любой сбой разбора превращались в пустой список.
/// А сохранение проекта устроено как «прочитать → добавить → записать целиком» —
/// значит, первое же действие пользователя перезаписывало файл ОДНИМ новым
/// проектом, и все прежние (с инструкциями и привязанными чатами) исчезали
/// безвозвратно. У клиента нет ни резервной копии, ни поддержки.
///
/// Достаточно было антивируса, на долю секунды залочившего файл в перемещаемом
/// профиле Windows, или одной ошибки диска.
fn load_projects(app: &tauri::AppHandle) -> Result<Vec<Project>, String> {
    let path = projects_path(app)?;
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        // Файла нет — проектов ещё не заводили. Это единственный отказ чтения,
        // который действительно означает «пусто».
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(format!(
                "Не удалось прочитать список проектов ({e}). Проекты НЕ потеряны — \
                 файл на месте: {}. Закройте программы, которые могут его держать \
                 (антивирус, синхронизация облака), и повторите.",
                path.display()
            ))
        }
    };
    if text.trim().is_empty() {
        return Ok(Vec::new()); // пустой файл — то же, что и отсутствующий
    }
    serde_json::from_str::<Vec<Project>>(&text).map_err(|e| {
        format!(
            "Список проектов повреждён и не читается ({e}). Чтобы не потерять его \
             остатки, программа ничего не перезаписывает. Файл: {}",
            path.display()
        )
    })
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

#[cfg(test)]
mod load_tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("jai-proj-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Та же логика, что в load_projects, но без AppHandle — проверяем разбор,
    /// а не путь к каталогу данных.
    fn load_from(path: &std::path::Path) -> Result<Vec<Project>, String> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(format!("не прочитать: {e}")),
        };
        if text.trim().is_empty() {
            return Ok(Vec::new());
        }
        serde_json::from_str::<Vec<Project>>(&text).map_err(|e| format!("повреждён: {e}"))
    }

    /// Повреждённый файл обязан быть ОШИБКОЙ, а не «проектов нет».
    ///
    /// Сохранение проекта устроено как «прочитать → добавить → записать целиком».
    /// Пока повреждение читалось как пустой список, первое же действие
    /// пользователя перезаписывало файл одним новым проектом — все прежние,
    /// с инструкциями и привязанными чатами, исчезали навсегда.
    #[test]
    fn corrupt_file_is_an_error_not_an_empty_list() {
        let dir = tmp("corrupt");
        let f = dir.join("projects.json");
        std::fs::write(&f, "{ это не тот json").unwrap();

        let err = load_from(&f).unwrap_err();
        assert!(err.contains("повреждён"), "невнятная причина: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// А отсутствие файла и пустой файл — честно «проектов ещё нет»: это первый
    /// запуск, и отказывать здесь было бы неверно.
    #[test]
    fn missing_or_empty_file_means_no_projects_yet() {
        let dir = tmp("empty");
        let missing = dir.join("projects.json");
        assert_eq!(load_from(&missing).unwrap().len(), 0);

        std::fs::write(&missing, "   \n").unwrap();
        assert_eq!(load_from(&missing).unwrap().len(), 0);

        std::fs::write(&missing, "[]").unwrap();
        assert_eq!(load_from(&missing).unwrap().len(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
