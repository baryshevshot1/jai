// ── Настройки приложения и офлайн-пути движка ───────────────────────────────
// settings.json (объект ключ→строка) в appDataDir; отсюда же читаются override-пути
// движка и каталога моделей, на которых работают ensure_engine/reload_engine.

use tauri::Manager;

use crate::engine;
use crate::error::AppResult;
use crate::write_atomic;

/// Сериализует доступ к settings.json. set_setting делает read-modify-write всего
/// файла; без этого лока две близкие записи (тема/модель/Thinking) могли бы прочитать
/// старое состояние и затереть ключи друг друга. Лок держим на время чтения+записи.
pub(crate) struct SettingsLock(pub(crate) std::sync::Mutex<()>);

// ── Хранение настроек: appDataDir/settings.json (объект ключ→строка) ─────────

fn settings_path(app: &tauri::AppHandle) -> Result<std::path::PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Не удалось получить appDataDir: {e}"))?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("Не удалось создать каталог: {e}"))?;
    Ok(dir.join("settings.json"))
}

pub(crate) fn read_settings(app: &tauri::AppHandle) -> serde_json::Map<String, serde_json::Value> {
    if let Ok(path) = settings_path(app) {
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(serde_json::Value::Object(map)) = serde_json::from_str(&text) {
                return map;
            }
        }
    }
    serde_json::Map::new()
}

#[tauri::command]
pub(crate) fn get_setting(app: tauri::AppHandle, key: String) -> Result<Option<String>, String> {
    let map = read_settings(&app);
    Ok(map.get(&key).and_then(|v| v.as_str()).map(String::from))
}

/// Запись одной настройки под мьютексом (общий помощник для set_setting и
/// команд override-путей). Лок на весь read-modify-write: параллельная запись
/// другого ключа не затрёт наш. into_inner — восстановление после «отравления».
fn write_setting(
    app: &tauri::AppHandle,
    lock: &SettingsLock,
    key: &str,
    value: &str,
) -> Result<(), String> {
    let _guard = lock.0.lock().unwrap_or_else(|e| e.into_inner());
    let mut map = read_settings(app);
    map.insert(key.to_string(), serde_json::Value::String(value.to_string()));
    let path = settings_path(app)?;
    let text = serde_json::to_string_pretty(&serde_json::Value::Object(map))
        .map_err(|e| e.to_string())?;
    write_atomic(&path, &text)
}

#[tauri::command]
pub(crate) fn set_setting(
    app: tauri::AppHandle,
    lock: tauri::State<'_, SettingsLock>,
    key: String,
    value: String,
) -> Result<(), String> {
    write_setting(&app, &lock, &key, &value)
}

/// Override-путь из настроек (для гибкого разрешения путей движка/моделей). Пустой
/// или отсутствующий → None. Заполняется в будущем офлайн-инсталлером.
pub(crate) fn read_setting_path(app: &tauri::AppHandle, key: &str) -> Option<std::path::PathBuf> {
    read_settings(app)
        .get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from)
}

// ── Офлайн-поставка: запись override-путей движка/моделей (Фаза упаковки) ─────
// Пишем в те же ключи, что читает ensure_engine (ollama_path / ollama_models_dir).

/// Задать путь к исполняемому файлу движка (для air-gapped, когда Ollama не в PATH).
#[tauri::command]
pub(crate) fn set_engine_path(
    app: tauri::AppHandle,
    lock: tauri::State<'_, SettingsLock>,
    path: String,
) -> Result<(), String> {
    engine::validate_engine_exe(std::path::Path::new(&path))?;
    write_setting(&app, &lock, "ollama_path", &path)
}

/// Задать путь к предзагруженному каталогу моделей Ollama (офлайн-поставка моделей).
#[tauri::command]
pub(crate) fn set_models_dir(
    app: tauri::AppHandle,
    lock: tauri::State<'_, SettingsLock>,
    path: String,
) -> Result<(), String> {
    engine::validate_models_dir(std::path::Path::new(&path))?;
    write_setting(&app, &lock, "ollama_models_dir", &path)
}

/// Сбросить override-пути → возврат к цепочке ресурс→PATH (пустые ключи = None).
#[tauri::command]
pub(crate) fn clear_engine_overrides(
    app: tauri::AppHandle,
    lock: tauri::State<'_, SettingsLock>,
) -> Result<(), String> {
    write_setting(&app, &lock, "ollama_path", "")?;
    write_setting(&app, &lock, "ollama_models_dir", "")
}

/// Обеспечить работу движка Ollama при старте: переиспользовать запущенный или
/// поднять свой (с OLLAMA_HOST/OLLAMA_MODELS), дождавшись готовности. Пути к движку
/// и моделям — гибко (override из настроек → ресурс рядом → PATH), без абсолютов.
#[tauri::command]
pub(crate) async fn ensure_engine(
    app: tauri::AppHandle,
    state: tauri::State<'_, engine::EngineState>,
) -> AppResult<engine::EngineStatus> {
    let exe_override = read_setting_path(&app, "ollama_path");
    let models_override = read_setting_path(&app, "ollama_models_dir");
    let resource_dir = app.path().resource_dir().ok();
    Ok(engine::ensure(&state, exe_override, models_override, resource_dir).await)
}

/// Применить актуальные override к движку: если движок наш — перезапустить его с
/// новым OLLAMA_MODELS; если внешний/системный — честно сообщить, что применится
/// при следующем нашем запуске (чужой экземпляр не трогаем). Без сети (офлайн-путь).
#[tauri::command]
pub(crate) async fn reload_engine(
    app: tauri::AppHandle,
    state: tauri::State<'_, engine::EngineState>,
) -> AppResult<engine::EngineStatus> {
    if engine::was_started_by_us(&state) {
        engine::stop_if_ours(&state); // гасит наш процесс и ждёт освобождения
    } else if engine::is_running().await {
        // системный/внешний движок — не трогаем; override вступит при нашем запуске
        return Ok(engine::EngineStatus {
            status: "external".into(),
            message: "Движок запущен системно — указанный каталог применится, когда \
                      движок поднимет само приложение (при запуске без системной Ollama)."
                .into(),
        });
    }
    let exe_override = read_setting_path(&app, "ollama_path");
    let models_override = read_setting_path(&app, "ollama_models_dir");
    let resource_dir = app.path().resource_dir().ok();
    Ok(engine::ensure(&state, exe_override, models_override, resource_dir).await)
}
