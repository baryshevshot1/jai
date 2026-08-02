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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const OLLAMA_URL: &str = "http://127.0.0.1:11434";
const OLLAMA_HOST: &str = "127.0.0.1:11434"; // движок всегда слушает только localhost
const READY_TRIES: u32 = 120; // 120 × 250мс ≈ 30 с — с запасом на холодный старт ollama serve
const READY_INTERVAL_MS: u64 = 250;
const MONITOR_INTERVAL_SECS: u64 = 45; // период присмотра за своим процессом движка

/// Состояние движка: подняли ли мы его сами и дескриптор дочернего процесса.
/// Хранится в Tauri State; на выходе по нему решаем, что останавливать.
/// exe/models_dir — параметры последнего запуска: присмотр перезапускает упавший
/// процесс ровно с ними, не перечитывая настройки.
#[derive(Default)]
struct Engine {
    started_by_us: bool,
    child: Option<Child>,
    exe: Option<PathBuf>,
    models_dir: Option<PathBuf>,
}

struct EngineInner {
    engine: Mutex<Engine>,
    /// Сериализация ensure(): без замка два одновременных вызова (старт приложения +
    /// «перезапустить» из настроек) оба видят «не жив», оба спаунят — и второй
    /// убивает здоровый процесс первого. std-мьютекс не годится: его нельзя держать
    /// через await опроса готовности.
    ensure_lock: tokio::sync::Mutex<()>,
    monitor_on: AtomicBool, // фоновый присмотр уже запущен (запускается один раз)
}

pub struct EngineState(Arc<EngineInner>);

impl EngineState {
    pub fn new() -> Self {
        EngineState(Arc::new(EngineInner {
            engine: Mutex::new(Engine::default()),
            ensure_lock: tokio::sync::Mutex::new(()),
            monitor_on: AtomicBool::new(false),
        }))
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
/// Общий клиент процесса (crate::HTTP): опрос готовности зовёт alive() десятки раз —
/// не пересоздаём клиент на каждый.
async fn alive() -> bool {
    crate::HTTP
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
    // Общесистемная установка (OllamaSetup /ALLUSERS от администратора) живёт в
    // Program Files, а свежедобавленный системный PATH до перелогина в GUI-процесс
    // может не попасть — проверяем каталог сами.
    for var in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(pf) = std::env::var_os(var) {
            v.push(PathBuf::from(pf).join("Ollama"));
        }
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
    // Один запуск за раз: конкурент дождётся и увидит уже живой движок (перепроверка
    // alive() ниже), а не спаунит второй процесс и не убьёт здоровый первый.
    let _serial = state.0.ensure_lock.lock().await;

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
    let old = {
        let mut g = state.0.engine.lock().unwrap_or_else(|e| e.into_inner());
        // прежний свой процесс (упавший — живой ответил бы выше) заберём и пожнём
        let old = g.child.take();
        g.child = Some(child);
        g.started_by_us = true;
        g.exe = Some(exe);
        g.models_dir = models_override;
        old
    }; // гард освобождаем ДО await ниже (не держим Mutex через await)
    if let Some(old) = old {
        // kill_tree ждёт до 3 с — не в воркере рантайма и не под мьютексом
        tauri::async_runtime::spawn_blocking(move || kill_tree(old));
    }

    // Ждём готовности: опрашиваем /api/version с интервалом и верхней границей.
    for _ in 0..READY_TRIES {
        if alive().await {
            start_monitor(state);
            return EngineStatus {
                status: "ready".into(),
                message: "Движок запущен".into(),
            };
        }
        tokio::time::sleep(Duration::from_millis(READY_INTERVAL_MS)).await;
    }

    // Не дождались: процесс упал либо порт держит кто-то ещё. Ребёнка пожинаем
    // (иначе зомби до конца сессии) и снимаем «наш» — оценки памяти по q8-профилю
    // не должны опираться на неработающий процесс.
    let dead = {
        let mut g = state.0.engine.lock().unwrap_or_else(|e| e.into_inner());
        g.started_by_us = false;
        g.exe = None;
        g.models_dir = None;
        g.child.take()
    };
    if let Some(c) = dead {
        tauri::async_runtime::spawn_blocking(move || kill_tree(c));
    }
    EngineStatus {
        status: "error".into(),
        message: "Движок не ответил вовремя (таймаут запуска).".into(),
    }
}

/// Фоновый присмотр за СВОИМ процессом движка: упавший `ollama serve` пожинается
/// (не висит зомби) и перезапускается с теми же параметрами — коробочный продукт
/// не должен ждать, пока пользователь наткнётся на ошибку чата. Внешний движок и
/// намеренную остановку (stop_if_ours снимает started_by_us) не трогаем.
fn start_monitor(state: &EngineState) {
    if state.0.monitor_on.swap(true, Ordering::SeqCst) {
        return; // уже присматриваем
    }
    let inner = state.0.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(MONITOR_INTERVAL_SECS)).await;
            // тот же замок, что у ensure(): не спорим с ручным перезапуском
            let _serial = inner.ensure_lock.lock().await;
            let respawn = {
                let mut g = inner.engine.lock().unwrap_or_else(|e| e.into_inner());
                if !g.started_by_us {
                    None
                } else {
                    match g.child.as_mut().map(|c| c.try_wait()) {
                        // умер — пожат самим try_wait; забираем параметры запуска
                        Some(Ok(Some(_))) => {
                            g.child = None;
                            g.exe.clone().map(|exe| (exe, g.models_dir.clone()))
                        }
                        _ => None, // жив (или дескриптора нет) — ничего не делаем
                    }
                }
            };
            let Some((exe, models_dir)) = respawn else { continue };
            if alive().await {
                // порт уже занял внешний экземпляр — нашего процесса больше нет
                let mut g = inner.engine.lock().unwrap_or_else(|e| e.into_inner());
                if g.child.is_none() {
                    g.started_by_us = false;
                }
                continue;
            }
            match spawn(&exe, models_dir.as_deref()) {
                Ok(child) => {
                    let mut g = inner.engine.lock().unwrap_or_else(|e| e.into_inner());
                    g.child = Some(child);
                }
                Err(_) => {
                    // перезапуск невозможен (движок удалили?) — больше не «наш»
                    let mut g = inner.engine.lock().unwrap_or_else(|e| e.into_inner());
                    g.started_by_us = false;
                }
            }
        }
    });
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

    #[cfg(target_os = "linux")]
    {
        // AppImage (AppRun linuxdeploy) экспортирует LD_LIBRARY_PATH и GTK/GStreamer-
        // переменные, указывающие внутрь смонтированного образа. Унаследовав их,
        // ollama serve подцепляет несовместимые libstdc++/libssl и падает — хотя из
        // терминала запускается. Системному движку эти переменные не нужны, вычищаем.
        for (key, _) in std::env::vars_os() {
            let name = key.to_string_lossy();
            if name == "LD_LIBRARY_PATH"
                || name == "LD_PRELOAD"
                || name == "PYTHONHOME"
                || name == "PYTHONPATH"
                || name.starts_with("GST_")
                || name.starts_with("GIO_")
            {
                cmd.env_remove(&key);
            }
        }
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // своя группа процессов → на выходе гасим всю группу (включая runner-процессы)
        cmd.process_group(0);
        // Пониженный приоритет (nice 5): под нагрузкой движок уступает интерфейсу
        // и системе — слабые машины меньше греются и не «замерзают», а на свободной
        // машине скорость почти не меняется. Наследуется runner-процессами Ollama.
        // Неудача понижения не должна мешать запуску — результат не проверяем.
        unsafe {
            cmd.pre_exec(|| {
                libc::setpriority(libc::PRIO_PROCESS as _, 0, 5);
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000; // без всплывающего окна консоли
        // Пониженный класс приоритета — та же цель, что nice на Unix: движок
        // уступает интерфейсу и системе, меньше перегрева на слабых машинах.
        const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
        cmd.creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS);
    }
    cmd.spawn()
}

/// Запускали ли движок мы сами (для решения «можно ли перезапустить с новым override»).
/// Мёртвый ребёнок — не «наш работающий движок»: отвечать на порту может внешний
/// экземпляр с обычным f16-KV, и экономный q8-профиль к нему не относится.
pub fn was_started_by_us(state: &EngineState) -> bool {
    let mut g = state.0.engine.lock().unwrap_or_else(|e| e.into_inner());
    if !g.started_by_us {
        return false;
    }
    match g.child.as_mut().map(|c| c.try_wait()) {
        Some(Ok(Some(_))) => false, // упал (присмотр пожнёт и перезапустит)
        Some(_) => true,            // жив либо опрос не удался — считаем нашим
        None => false,              // дескриптора нет (перезапуск не удался)
    }
}

/// Жив ли движок сейчас (публичная обёртка над проверкой /api/version).
pub async fn is_running() -> bool {
    alive().await
}

/// Остановить движок, ЕСЛИ его запустили мы. Внешний/системный — не трогаем.
/// kill_tree ждёт до 3 с — выполняем его уже ОТПУСТИВ мьютекс, чтобы параллельные
/// was_started_by_us/ensure не подвисали на это время.
pub fn stop_if_ours(state: &EngineState) {
    let child = {
        let mut g = state.0.engine.lock().unwrap_or_else(|e| e.into_inner());
        if !g.started_by_us {
            return;
        }
        g.started_by_us = false;
        g.exe = None;
        g.models_dir = None;
        g.child.take()
    };
    if let Some(child) = child {
        kill_tree(child);
    }
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
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let pid = child.id();
    // /T — всё дерево процессов, /F — принудительно. CREATE_NO_WINDOW — иначе при
    // выходе из GUI-приложения на миг вспыхивает чёрное окно консоли taskkill.
    let _ = Command::new("taskkill")
        .args(["/T", "/F", "/PID", &pid.to_string()])
        .creation_flags(CREATE_NO_WINDOW)
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
