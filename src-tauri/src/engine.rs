// ── Управление процессом движка Ollama (операционный слой) ───────────────────
//
// Приложение само обеспечивает работу Ollama: переиспользует уже запущенный движок
// (системный/ручной) ЛИБО поднимает свой и аккуратно гасит его при выходе — но
// останавливает ТОЛЬКО то, что запустило само (Принцип 1). Чужой/системный экземпляр
// не трогаем — от него могут зависеть другие программы.
//
// Путь к движку и каталог моделей разрешаются гибко, без зашитых абсолютных путей
// (Принцип 2): override из настроек → ресурс рядом с приложением → системный PATH.
// Это задел под будущую офлайн-поставку: инсталлер кладёт движок рядом (resource)
// или в произвольное место и пишет путь в настройку — логика запуска не меняется.

use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

const OLLAMA_URL: &str = "http://127.0.0.1:11434";
const OLLAMA_HOST: &str = "127.0.0.1:11434"; // движок всегда слушает только localhost
const READY_TRIES: u32 = 120; // 120 × 250мс ≈ 30 с — с запасом на холодный старт ollama serve
const READY_INTERVAL_MS: u64 = 250;

/// Состояние движка: подняли ли мы его сами и дескриптор дочернего процесса.
/// Хранится в Tauri State; на выходе по нему решаем, что останавливать.
#[derive(Default)]
struct Engine {
    started_by_us: bool,
    child: Option<Child>,
}

pub struct EngineState(Mutex<Engine>);

impl EngineState {
    pub fn new() -> Self {
        EngineState(Mutex::new(Engine::default()))
    }
}

/// Итог «обеспечения движка» для интерфейса.
/// status: "ready" — движок работает (свой или внешний); "not_installed" — не найден;
/// "error" — найден, но не удалось запустить / не дождались готовности.
#[derive(Serialize)]
pub struct EngineStatus {
    pub status: String,
    pub message: String,
}

/// Жив ли движок: GET /api/version с коротким таймаутом (та же проверка, что в UI).
async fn alive() -> bool {
    let client = reqwest::Client::new();
    client
        .get(format!("{OLLAMA_URL}/api/version"))
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

/// Имя исполняемого файла движка по ОС.
fn exe_name() -> &'static str {
    if cfg!(windows) {
        "ollama.exe"
    } else {
        "ollama"
    }
}

/// Разрешение пути к движку по приоритетной цепочке (Принцип 2). None → не найден.
fn resolve_exe(exe_override: Option<PathBuf>, resource_dir: Option<PathBuf>) -> Option<PathBuf> {
    // 1) явный override из настроек (для будущей офлайн-поставки)
    if let Some(p) = exe_override {
        if p.is_file() {
            return Some(p);
        }
    }
    // 2) бинарь, поставленный рядом с приложением (resource dir)
    if let Some(dir) = resource_dir {
        let cand = dir.join(exe_name());
        if cand.is_file() {
            return Some(cand);
        }
    }
    // 3) системный PATH
    if let Some(p) = find_in_path(exe_name()) {
        return Some(p);
    }
    // 3б) типичные места установки (GUI-приложению PATH могут не пробросить)
    for dir in well_known_dirs() {
        let cand = dir.join(exe_name());
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join(name);
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn well_known_dirs() -> Vec<PathBuf> {
    vec![
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/Applications/Ollama.app/Contents/Resources"),
    ]
}
#[cfg(target_os = "linux")]
fn well_known_dirs() -> Vec<PathBuf> {
    vec![PathBuf::from("/usr/local/bin"), PathBuf::from("/usr/bin")]
}
#[cfg(target_os = "windows")]
fn well_known_dirs() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        v.push(PathBuf::from(local).join("Programs").join("Ollama"));
    }
    v
}
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn well_known_dirs() -> Vec<PathBuf> {
    Vec::new()
}

// ── Валидация override-путей (офлайн-поставка) ───────────────────────────────

/// Проверяет, что путь — исполняемый файл движка (для override `ollama_path`).
pub fn validate_engine_exe(p: &Path) -> Result<(), String> {
    if !p.is_file() {
        return Err("Файл движка не найден по указанному пути".to_string());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(p)
            .map_err(|e| format!("Не удалось прочитать файл: {e}"))?
            .permissions()
            .mode();
        if mode & 0o111 == 0 {
            return Err("Указанный файл не исполняемый (нет права на выполнение)".to_string());
        }
    }
    #[cfg(windows)]
    {
        let is_exe = p
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("exe"))
            .unwrap_or(false);
        if !is_exe {
            return Err("Ожидается исполняемый файл .exe".to_string());
        }
    }
    Ok(())
}

/// Проверяет, что каталог похож на каталог моделей Ollama: внутри есть `manifests`
/// и `blobs` (для override `ollama_models_dir`). Так отсекаем случайные папки.
pub fn validate_models_dir(p: &Path) -> Result<(), String> {
    if !p.is_dir() {
        return Err("Каталог моделей не найден по указанному пути".to_string());
    }
    if !p.join("manifests").is_dir() || !p.join("blobs").is_dir() {
        return Err(
            "Это не похоже на каталог моделей Ollama (нужны подкаталоги manifests и blobs)"
                .to_string(),
        );
    }
    Ok(())
}

/// Обеспечить работу движка. Возвращает статус для интерфейса; при запуске своего
/// процесса сохраняет его дескриптор в EngineState (для остановки на выходе).
pub async fn ensure(
    state: &EngineState,
    exe_override: Option<PathBuf>,
    models_override: Option<PathBuf>,
    resource_dir: Option<PathBuf>,
) -> EngineStatus {
    // Уже отвечает (системный/ручной/ранее поднятый нами) — переиспользуем, не трогаем.
    if alive().await {
        return EngineStatus {
            status: "ready".into(),
            message: "Движок уже работает".into(),
        };
    }

    // Не отвечает — ищем исполняемый файл движка.
    let exe = match resolve_exe(exe_override, resource_dir) {
        Some(p) => p,
        None => {
            return EngineStatus {
                status: "not_installed".into(),
                message: "Движок Ollama не найден. Установите Ollama и перезапустите приложение."
                    .into(),
            };
        }
    };

    // Запускаем свой процесс и запоминаем дескриптор.
    let child = match spawn(&exe, models_override.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            return EngineStatus {
                status: "error".into(),
                message: format!("Не удалось запустить движок: {e}"),
            };
        }
    };
    {
        let mut g = state.0.lock().unwrap_or_else(|e| e.into_inner());
        // если в этой сессии уже поднимали свой движок — погасим прежний, не плодим
        if let Some(old) = g.child.take() {
            kill_tree(old);
        }
        g.child = Some(child);
        g.started_by_us = true;
    } // гард освобождаем ДО await ниже (не держим Mutex через await)

    // Ждём готовности: опрашиваем /api/version с интервалом и верхней границей.
    for _ in 0..READY_TRIES {
        if alive().await {
            return EngineStatus {
                status: "ready".into(),
                message: "Движок запущен".into(),
            };
        }
        tokio::time::sleep(Duration::from_millis(READY_INTERVAL_MS)).await;
    }
    EngineStatus {
        status: "error".into(),
        message: "Движок не ответил вовремя (таймаут запуска).".into(),
    }
}

fn spawn(exe: &Path, models_dir: Option<&Path>) -> std::io::Result<Child> {
    let mut cmd = Command::new(exe);
    cmd.arg("serve");
    cmd.env("OLLAMA_HOST", OLLAMA_HOST); // явно только localhost (правило проекта)

    // Экономный режим (защита от зависания-от-свопа). Действует, только когда движок
    // поднимает само приложение; внешнему/системному экземпляру не навязываем.
    // - MAX_LOADED_MODELS=1: НЕ держать несколько моделей в памяти одновременно —
    //   главный фикс «маленький запрос уронил систему» (чат + bge-m3 после RAG +
    //   зрение давали суммарный объём за пределы RAM → своп → зависание).
    // - NUM_PARALLEL=1: не умножать память на параллельные запросы.
    // - FLASH_ATTENTION + KV_CACHE_TYPE=q8_0: KV-кэш примерно вдвое меньше.
    // - KEEP_ALIVE: умеренный (точка тонкой настройки); эмбеддинги уже выгружаются за 30с.
    cmd.env("OLLAMA_MAX_LOADED_MODELS", "1");
    cmd.env("OLLAMA_NUM_PARALLEL", "1");
    cmd.env("OLLAMA_FLASH_ATTENTION", "1");
    cmd.env("OLLAMA_KV_CACHE_TYPE", "q8_0");
    cmd.env("OLLAMA_KEEP_ALIVE", "5m");

    match models_dir {
        // явный override каталога моделей (для будущей офлайн-поставки)
        Some(dir) => {
            cmd.env("OLLAMA_MODELS", dir);
        }
        // override не задан → НЕ наследуем случайную OLLAMA_MODELS из окружения
        // запускающей оболочки (она может указывать не туда). Убираем её, чтобы
        // Ollama использовала свой каталог по умолчанию — как системный экземпляр.
        None => {
            cmd.env_remove("OLLAMA_MODELS");
        }
    }
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::null());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // своя группа процессов → на выходе гасим всю группу (включая runner-процессы)
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW); // без всплывающего окна консоли
    }
    cmd.spawn()
}

/// Запускали ли движок мы сами (для решения «можно ли перезапустить с новым override»).
pub fn was_started_by_us(state: &EngineState) -> bool {
    state.0.lock().unwrap_or_else(|e| e.into_inner()).started_by_us
}

/// Жив ли движок сейчас (публичная обёртка над проверкой /api/version).
pub async fn is_running() -> bool {
    alive().await
}

/// Остановить движок, ЕСЛИ его запустили мы. Внешний/системный — не трогаем.
pub fn stop_if_ours(state: &EngineState) {
    let mut g = state.0.lock().unwrap_or_else(|e| e.into_inner());
    if !g.started_by_us {
        return;
    }
    if let Some(child) = g.child.take() {
        kill_tree(child);
    }
    g.started_by_us = false;
}

/// Гасит процесс СО ВСЕМИ дочерними (Ollama порождает runner-процессы): сначала
/// мягко, затем принудительно. Unix — по группе процессов; Windows — дерево.
#[cfg(unix)]
fn kill_tree(mut child: Child) {
    let pid = child.id() as i32;
    // отрицательный pid = вся группа процессов (ловим и runner-ы, не только serve)
    unsafe {
        libc::kill(-pid, libc::SIGTERM);
    }
    for _ in 0..30 {
        match child.try_wait() {
            Ok(Some(_)) => return, // завершился мягко
            _ => std::thread::sleep(Duration::from_millis(100)),
        }
    }
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
    let _ = child.wait();
}

#[cfg(windows)]
fn kill_tree(mut child: Child) {
    let pid = child.id();
    // /T — всё дерево процессов, /F — принудительно
    let _ = Command::new("taskkill")
        .args(["/T", "/F", "/PID", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("jai_eng_{}_{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    // Каталог моделей опознаётся только по наличию manifests/ и blobs/.
    #[test]
    fn models_dir_validation() {
        let d = tmp("models");
        assert!(validate_models_dir(&d).is_err(), "пустой каталог — не модели");
        fs::create_dir_all(d.join("manifests")).unwrap();
        fs::create_dir_all(d.join("blobs")).unwrap();
        assert!(validate_models_dir(&d).is_ok(), "manifests+blobs — это модели");
        assert!(validate_models_dir(&d.join("nope")).is_err(), "несуществующий путь");
        let _ = fs::remove_dir_all(&d);
    }

    // Исполняемый файл движка: Unix — по биту исполнения; Windows — по .exe.
    #[test]
    fn engine_exe_validation() {
        let d = tmp("engine");
        let f = d.join(exe_name());
        fs::write(&f, b"x").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&f, fs::Permissions::from_mode(0o644)).unwrap();
            assert!(validate_engine_exe(&f).is_err(), "без бита исполнения — ошибка");
            fs::set_permissions(&f, fs::Permissions::from_mode(0o755)).unwrap();
            assert!(validate_engine_exe(&f).is_ok(), "с битом исполнения — ок");
        }
        #[cfg(windows)]
        {
            assert!(validate_engine_exe(&f).is_ok());
            let bad = d.join("ollama.txt");
            fs::write(&bad, b"x").unwrap();
            assert!(validate_engine_exe(&bad).is_err(), "не .exe — ошибка");
        }
        assert!(validate_engine_exe(&d.join("nope")).is_err(), "несуществующий файл");
        let _ = fs::remove_dir_all(&d);
    }
}
