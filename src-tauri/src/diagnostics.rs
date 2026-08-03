// ── Железо и диагностика: детект ресурсов + «проверка системы» ──────────────
// Локальные системные вызовы, без сети: сколько памяти и видеопамяти у машины,
// жив ли движок, всё ли на месте для установки под ключ.

use serde::Serialize;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::Manager;

use crate::docstore;
use crate::models::model_states;
use crate::HTTP;

// ── Штамп сборки: что именно сейчас запущено ─────────────────────────────────
//
// Появился после сессии, где симптомы двух разных бандлов сравнивались как симптомы
// одного. Пока «какая это сборка» — вопрос к памяти разработчика, любой отчёт об
// отказе стоит ровно столько же, сколько эта память.

/// Что известно про запущенный бинарник. Собирается из штампа build.rs и из
/// фактического состояния машины — не из предположений.
#[derive(Serialize)]
pub(crate) struct BuildInfo {
    version: String,
    git_sha: String,
    git_dirty: String,
    profile: String,
    target: String,
    /// Собранные cargo-фичи: голосовой ввод можно выключить, и «кнопки нет» тогда
    /// не поломка, а свойство сборки. Без этой строки одно принимают за другое.
    features: Vec<String>,
    /// Команды, реально зарегистрированные в этом бинарнике.
    commands: Vec<String>,
    os: String,
    arch: String,
    /// Модель распознавания: где ждём, что там лежит, и какой файл считаем верным.
    voice_model_path: String,
    voice_model_present: bool,
    voice_model_partial_bytes: u64,
    voice_model_expected_bytes: u64,
    voice_model_sha256: String,
    log_path: Option<String>,
}

#[tauri::command]
pub(crate) fn build_info(app: tauri::AppHandle) -> BuildInfo {
    // cfg!(), а не #[cfg] с push: значение вычисляется на этапе компиляции так же,
    // но список остаётся одним выражением — добавить фичу и забыть про строку нельзя.
    let features: Vec<String> = [
        cfg!(feature = "voice").then_some("voice"),
        cfg!(feature = "devtools").then_some("devtools"),
    ]
    .into_iter()
    .flatten()
    .map(str::to_string)
    .collect();

    let model = crate::voice::model_path(&app).ok();
    let present = model.as_ref().map(|p| p.is_file()).unwrap_or(false);
    let partial = model
        .as_ref()
        .map(|p| {
            let mut part = p.as_os_str().to_os_string();
            part.push(".part");
            std::fs::metadata(std::path::PathBuf::from(part)).map(|m| m.len()).unwrap_or(0)
        })
        .unwrap_or(0);

    BuildInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        git_sha: env!("JAI_GIT_SHA").to_string(),
        git_dirty: env!("JAI_GIT_DIRTY").to_string(),
        profile: env!("JAI_BUILD_PROFILE").to_string(),
        target: env!("JAI_BUILD_TARGET").to_string(),
        features,
        commands: crate::COMMAND_NAMES.iter().map(|s| s.to_string()).collect(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        voice_model_path: model
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
        voice_model_present: present,
        voice_model_partial_bytes: if present { 0 } else { partial },
        voice_model_expected_bytes: crate::voice::MODEL_BYTES,
        voice_model_sha256: crate::voice::MODEL_SHA256.to_string(),
        log_path: crate::journal::journal_path(),
    }
}

/// Самопроверка собранного приложения: `jai --self-check`.
///
/// Отвечает на вопрос, который иначе задаётся человеку и человеком же угадывается:
/// «что это за сборка и жива ли она». Печатает JSON и завершает процесс — ненулевым
/// кодом, если что-то из необходимого недоступно. Этим пользуется гейт (smoke-тест
/// бандла: собрали → запустили → прочитали код возврата) и этим же можно проверить
/// установку у клиента, не открывая окна.
///
/// Чего она НЕ проверяет и не может: что окно нарисовалось и что интерфейс жив. Это
/// закрывается только глазами — см. чек-лист ручной проверки в docs/RELEASE.md.
pub(crate) fn self_check(app: &tauri::AppHandle) -> i32 {
    let info = build_info(app.clone());
    let mut problems: Vec<String> = Vec::new();

    // Каталоги, без которых приложение не сможет ни хранить, ни жаловаться.
    let mut dirs = serde_json::Map::new();
    for (name, dir) in [
        ("data", app.path().app_data_dir()),
        ("config", app.path().app_config_dir()),
        ("log", app.path().app_log_dir()),
    ] {
        match dir {
            Ok(d) => {
                let writable = std::fs::create_dir_all(&d).is_ok();
                if !writable {
                    problems.push(format!("каталог «{name}» недоступен для записи: {}", d.display()));
                }
                dirs.insert(name.to_string(), serde_json::json!(d.to_string_lossy()));
            }
            Err(e) => {
                problems.push(format!("каталог «{name}» не определён: {e}"));
            }
        }
    }

    if info.commands.is_empty() {
        problems.push("в сборке не зарегистрировано ни одной команды".into());
    }
    // Голос — фича, и его отсутствие НЕ ошибка: сборка без него честна и работоспособна.
    // Но знать об этом надо явно, а не выяснять по отсутствующей кнопке.
    let voice_built = info.features.iter().any(|f| f == "voice");

    let report = serde_json::json!({
        "ok": problems.is_empty(),
        "build": {
            "version": info.version,
            "git_sha": info.git_sha,
            "git_dirty": info.git_dirty,
            "profile": info.profile,
            "target": info.target,
        },
        "features": info.features,
        "voice_built": voice_built,
        "commands": info.commands.len(),
        "command_names": info.commands,
        "os": info.os,
        "arch": info.arch,
        "dirs": dirs,
        "voice_model": {
            "path": info.voice_model_path,
            "installed": info.voice_model_present,
            "partial_bytes": info.voice_model_partial_bytes,
            "expected_bytes": info.voice_model_expected_bytes,
            "sha256": info.voice_model_sha256,
        },
        "log_path": info.log_path,
        "problems": problems,
    });
    println!("{}", serde_json::to_string_pretty(&report).unwrap_or_default());
    if problems.is_empty() {
        0
    } else {
        1
    }
}

/// Версия Ollama: GET 127.0.0.1:11434/api/version — мягкая проверка «движок жив».
/// Таймаут 3 с, чтобы не зависнуть, если порт открыт, но ответа нет. Только localhost.
#[tauri::command]
pub(crate) async fn ollama_version() -> Result<String, String> {
    let resp = HTTP
        .get("http://127.0.0.1:11434/api/version")
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await
        .map_err(|e| format!("Не удалось подключиться к Ollama: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("Ollama вернул ошибку {}", resp.status()));
    }

    let json: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    let version = json
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("неизвестно")
        .to_string();
    Ok(version)
}

/// Сведения о железе для «светофора». Считается локально, без сети.
#[derive(Serialize)]
pub(crate) struct HardwareInfo {
    ram_gb: f64,
    cpu_cores: usize,
    vram_gb: Option<f64>,      // всего (класс железа — стабилен)
    vram_free_gb: Option<f64>, // свободно сейчас (честная доступность)
    vram_source: String, // "dxgi" | "nvidia-smi" | "rocm-smi" | "unified" | "unknown"
    tier: String,        // "green" | "yellow" | "red"
}

/// Детект ресурсов машины. Команда синхронная: внутри блокирующие вызовы
/// (sysinfo, запуск утилит), а Tauri выполняет sync-команды в пуле потоков —
/// главный поток не блокируется. Только локальные системные вызовы, без сети.
#[tauri::command]
pub(crate) fn detect_hardware() -> Result<HardwareInfo, String> {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    let ram_gb = sys.total_memory() as f64 / 1024.0 / 1024.0 / 1024.0;
    let cpu_cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(0);

    let (vram_gb, vram_free_gb, vram_source) = detect_vram();

    // Пороги «светофора» (по требованию): VRAM важнее, но RAM тоже учитываем.
    // Уровень — по ОБЪЁМУ видеопамяти (класс железа), не по свободной: открытый
    // браузер не должен «перекрашивать» машину из зелёной в жёлтую.
    let has_vram = |min: f64| vram_gb.is_some_and(|v| v >= min);
    let tier = if has_vram(6.0) || ram_gb >= 16.0 {
        "green"
    } else if has_vram(4.0) || ram_gb >= 8.0 {
        "yellow"
    } else {
        "red"
    }
    .to_string();

    Ok(HardwareInfo {
        ram_gb,
        cpu_cores,
        vram_gb,
        vram_free_gb,
        vram_source,
        tier,
    })
}

// ── Кэш детекта видеопамяти ────────────────────────────────────────────────
// plan_inference зовёт detect_vram() перед КАЖДЫМ сообщением чата (src/chat.ts), а
// сам детект на Linux/Windows — это запуск внешней утилиты (nvidia-smi/rocm-smi) или
// перечисление адаптеров DXGI: сотни мс, а без установленных драйверов — секунды.
// Объём видеопамяти — паспортная характеристика железа (кэшируем минутами);
// свободная часть скачет постоянно (кэшируем единицы секунд — то же правило, что и
// для живых данных о памяти в memory.rs: дольше нельзя, иначе защита от переполнения
// «отстанет» от реальности).

/// Свободная видеопамять — единицы секунд: покрывает быстрый обмен сообщениями в
/// одном диалоге, не даёт плану переполнения проглядеть память, занятую только что.
const VRAM_LIVE_TTL: Duration = Duration::from_secs(5);
/// Объём видеопамяти в рамках одного запуска приложения не меняется — можно держать
/// минутами (целевое железо — не горячая замена GPU, см. CLAUDE.md).
const VRAM_STATIC_TTL: Duration = Duration::from_secs(5 * 60);
/// Утилита детекта не должна вешать ответ: если процесс завис (битый /proc, залипший
/// драйвер), таймаут превращает зависание в честное «источник неизвестен», а не в
/// подвисший ответ модели. Используется только веткой Linux (nvidia-smi/rocm-smi) —
/// на других ОС детект не запускает внешний процесс.
#[cfg(target_os = "linux")]
const VRAM_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

struct VramCache {
    total: Option<f64>,
    free: Option<f64>,
    source: String,
    fetched_at: Instant,
}

static VRAM_CACHE: Mutex<Option<VramCache>> = Mutex::new(None);

/// Решение «отдать закэшированное или переизмерить» — чистая функция от возраста
/// записи и природы источника (вынесена отдельно ради теста без реального Instant/сна).
fn vram_cache_fresh(elapsed: Duration, source: &str, free: Option<f64>) -> bool {
    if elapsed < VRAM_LIVE_TTL {
        return true;
    }
    // Источники, которые «свободно» в принципе не отдают (rocm-smi не умеет,
    // unified/unknown — там его просто нет): второе число не может измениться само по
    // себе, значит нет смысла перезапускать утилиту чаще, чем раз в VRAM_STATIC_TTL —
    // объём всё равно тот же самый.
    let never_reports_free = free.is_none() && matches!(source, "rocm-smi" | "unified" | "unknown");
    never_reports_free && elapsed < VRAM_STATIC_TTL
}

/// Видеопамять машины: объём + свободно сейчас + источник детекта. Обёртка над
/// detect_vram_uncached() с кэшем (см. vram_cache_fresh) — сам детект платформенный,
/// ниже по файлу.
pub(crate) fn detect_vram() -> (Option<f64>, Option<f64>, String) {
    {
        let guard = VRAM_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = guard.as_ref() {
            if vram_cache_fresh(c.fetched_at.elapsed(), &c.source, c.free) {
                return (c.total, c.free, c.source.clone());
            }
        }
    }
    let (total, free, source) = detect_vram_uncached();
    let mut guard = VRAM_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some(VramCache { total, free, source: source.clone(), fetched_at: Instant::now() });
    (total, free, source)
}

/// Запускает команду с ограничением по времени: не полагаемся на то, что nvidia-smi/
/// rocm-smi всегда быстрые — если процесс не уложился, убиваем его и честно отдаём
/// None, а не блокируем поток на неопределённое время. Доступна и под `cfg(test)` —
/// таймаут проверяется на обычной команде (`sh -c sleep`), без реального nvidia-smi.
#[cfg(any(target_os = "linux", test))]
fn output_with_timeout(
    mut cmd: std::process::Command,
    timeout: Duration,
) -> Option<std::process::Output> {
    use std::io::Read;
    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    // Стдаут читаем в фоновом потоке: поллинг try_wait() иначе мог бы дедлокнуться,
    // допиши утилита в pipe больше системного буфера и заблокируйся сама на записи.
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });

    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait(); // не оставляем зомби-процесс
                    return None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let stdout = rx.recv_timeout(Duration::from_secs(1)).unwrap_or_default();
    Some(std::process::Output { status, stdout, stderr: Vec::new() })
}

// macOS: общая (unified) память — отдельной VRAM нет, уровень считаем по RAM.
// Возвращаемая тройка везде: (всего, свободно, источник).
#[cfg(target_os = "macos")]
fn detect_vram_uncached() -> (Option<f64>, Option<f64>, String) {
    (None, None, "unified".to_string())
}

// Windows: DXGI — поле DedicatedVideoMemory. Намеренно НЕ WMI AdapterRAM
// (он режет видеопамять до 4 ГБ). Берём максимум по адаптерам; у лучшего адаптера
// дополнительно спрашиваем СВОБОДНУЮ видеопамять через IDXGIAdapter3 (budget минус
// уже занятое) — честная доступность, а не паспортный объём.
#[cfg(target_os = "windows")]
fn detect_vram_uncached() -> (Option<f64>, Option<f64>, String) {
    use windows::core::Interface;
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, IDXGIAdapter3, IDXGIFactory1, DXGI_ERROR_NOT_FOUND,
        DXGI_MEMORY_SEGMENT_GROUP_LOCAL, DXGI_QUERY_VIDEO_MEMORY_INFO,
    };
    unsafe {
        let factory: IDXGIFactory1 = match CreateDXGIFactory1() {
            Ok(f) => f,
            Err(_) => return (None, None, "unknown".to_string()),
        };
        let mut best: u64 = 0;
        let mut best_free: Option<u64> = None;
        let mut i = 0u32;
        loop {
            match factory.EnumAdapters(i) {
                Ok(adapter) => {
                    // windows 0.58: GetDesc() возвращает дескриптор (не out-параметр)
                    if let Ok(desc) = adapter.GetDesc() {
                        let total = desc.DedicatedVideoMemory as u64;
                        if total > best {
                            best = total;
                            best_free = adapter.cast::<IDXGIAdapter3>().ok().and_then(|a3| {
                                let mut info = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
                                a3.QueryVideoMemoryInfo(
                                    0,
                                    DXGI_MEMORY_SEGMENT_GROUP_LOCAL,
                                    &mut info,
                                )
                                .ok()
                                .map(|_| info.Budget.saturating_sub(info.CurrentUsage))
                            });
                        }
                    }
                    i += 1;
                }
                Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(_) => break,
            }
        }
        if best > 0 {
            let gb = |b: u64| b as f64 / 1024.0 / 1024.0 / 1024.0;
            (Some(gb(best)), best_free.map(gb), "dxgi".to_string())
        } else {
            (None, None, "unknown".to_string())
        }
    }
}

// Linux: nvidia-smi → rocm-smi → неизвестно. Утилиты могут отсутствовать (ошибки
// гасим, не паникуем) или зависнуть (VRAM_PROBE_TIMEOUT) — сети нет.
#[cfg(target_os = "linux")]
fn detect_vram_uncached() -> (Option<f64>, Option<f64>, String) {
    use std::process::Command;

    // NVIDIA: одним вызовом total и free (МиБ), строка на GPU: "8192, 6144".
    // Берём GPU с наибольшим объёмом; free — с той же строки (тот же GPU).
    let mut nvidia = Command::new("nvidia-smi");
    nvidia.args(["--query-gpu=memory.total,memory.free", "--format=csv,noheader,nounits"]);
    if let Some(out) = output_with_timeout(nvidia, VRAM_PROBE_TIMEOUT) {
        if out.status.success() {
            let mut best: Option<(f64, f64)> = None; // (total, free) МиБ лучшего GPU
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                let mut it = line.split(',').map(str::trim);
                if let (Some(t), Some(f)) = (it.next(), it.next()) {
                    if let (Ok(t), Ok(f)) = (t.parse::<f64>(), f.parse::<f64>()) {
                        if best.is_none_or(|(bt, _)| t > bt) {
                            best = Some((t, f));
                        }
                    }
                }
            }
            if let Some((t, f)) = best {
                if t > 0.0 {
                    return (Some(t / 1024.0), Some(f / 1024.0), "nvidia-smi".to_string());
                }
            }
        }
    }

    // AMD: rocm-smi --showmeminfo vram --csv. Эвристика: наибольшее число байт
    // в выводе = суммарная VRAM. Свободную честно не знаем → None.
    let mut rocm = Command::new("rocm-smi");
    rocm.args(["--showmeminfo", "vram", "--csv"]);
    if let Some(out) = output_with_timeout(rocm, VRAM_PROBE_TIMEOUT) {
        if out.status.success() {
            let max_bytes = String::from_utf8_lossy(&out.stdout)
                .split(|c: char| !c.is_ascii_digit())
                .filter_map(|t| t.parse::<u64>().ok())
                .max()
                .unwrap_or(0);
            if max_bytes > 0 {
                return (
                    Some(max_bytes as f64 / 1024.0 / 1024.0 / 1024.0),
                    None,
                    "rocm-smi".to_string(),
                );
            }
        }
    }

    (None, None, "unknown".to_string())
}

// Прочие ОС: видеопамять неизвестна, ориентируемся на RAM.
#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
fn detect_vram_uncached() -> (Option<f64>, Option<f64>, String) {
    (None, None, "unknown".to_string())
}

// ── Диагностика («проверка системы»): самопроверка для установки под ключ ────
// Агрегирует уже существующие проверки (движок, модели, база, железо) + место на
// диске и доступность хранилища. Всё локально, без интернета; команда не падает —
// каждая проверка честно отчитывается сама за себя.

/// Минимальная версия Ollama — наибольшее из требований (безопасность ≥ 0.17.1;
/// зрение qwen3-vl ≥ 0.12.7). Ориентир актуальной ветки — 0.30.x (см. CLAUDE.md).
const MIN_OLLAMA_VERSION: [u64; 3] = [0, 17, 1];
/// Пороги свободного места на томе с данными: одна модель 8–9B (Q4_K_M) — ~5–6 ГБ,
/// поэтому < 15 ГБ — «впритык» (обновление модели может не пролезть), < 3 ГБ — беда.
const DISK_WARN_GB: f64 = 15.0;
const DISK_FAIL_GB: f64 = 3.0;

/// Итог одной проверки. status: "ok" | "warn" | "fail".
#[derive(Serialize)]
pub(crate) struct DiagCheck {
    id: &'static str,
    title: &'static str,
    status: &'static str,
    detail: String,
}

fn diag(id: &'static str, title: &'static str, status: &'static str, detail: String) -> DiagCheck {
    DiagCheck { id, title, status, detail }
}

/// "0.30.1" → [0, 30, 1]. Нечисловые хвосты компонентов ("1-rc2") отбрасываются,
/// недостающие компоненты = 0 — сравнение с минимумом не падает на экзотике.
fn parse_version(v: &str) -> [u64; 3] {
    let mut out = [0u64; 3];
    for (i, part) in v.trim().split('.').take(3).enumerate() {
        let digits: String = part.chars().take_while(|c| c.is_ascii_digit()).collect();
        out[i] = digits.parse().unwrap_or(0);
    }
    out
}

/// Полная самопроверка системы: движок, модели набора, база документов, место на
/// диске, железо, хранилище данных. Порядок фиксированный — фронт рисует как есть.
#[tauri::command]
pub(crate) async fn run_diagnostics(
    app: tauri::AppHandle,
    engine_state: tauri::State<'_, crate::engine::EngineState>,
) -> crate::error::AppResult<Vec<DiagCheck>> {
    let mut out = Vec::new();

    // 1. Движок: отвечает ли, и не старше ли минимально поддерживаемой версии.
    let engine_alive = match ollama_version().await {
        Ok(v) => {
            if parse_version(&v) < MIN_OLLAMA_VERSION {
                let min = MIN_OLLAMA_VERSION.map(|n| n.to_string()).join(".");
                out.push(diag("engine", "Движок Ollama", "warn",
                    format!("Работает, но версия {v} устарела — нужна не ниже {min}")));
            } else {
                out.push(diag("engine", "Движок Ollama", "ok", format!("Работает, версия {v}")));
            }
            true
        }
        Err(e) => {
            out.push(diag("engine", "Движок Ollama", "fail", format!("Не отвечает: {e}")));
            false
        }
    };

    // 1б. ЧЕЙ движок. Экономные настройки памяти (сколько моделей держать, каким
    // держать KV-кэш) приложение задаёт переменными окружения при ЗАПУСКЕ движка.
    // Системный экземпляр мы не трогаем — значит он работает с заводскими
    // умолчаниями, где моделей в памяти больше, а KV-кэш вдвое толще. На машине с
    // ограниченной памятью это и есть разница между «работает» и «всё в свопе».
    // Пользователь и поддержка должны это видеть, а не гадать.
    if engine_alive && !crate::engine::was_started_by_us(&engine_state) {
        out.push(diag(
            "engine-owner",
            "Режим экономии памяти",
            "warn",
            "Движок запущен системой, а не приложением — экономный профиль памяти к \
             нему не применён. Если приложение подтормаживает или переполняется \
             память, остановите системную службу Ollama: приложение поднимет свой \
             экземпляр с бережными настройками."
                .to_string(),
        ));
    } else if engine_alive {
        out.push(diag(
            "engine-owner",
            "Режим экономии памяти",
            "ok",
            "Движок запущен приложением — бережные настройки памяти применены".to_string(),
        ));
    }

    // 2. Модели набора: обязательные должны стоять (список — без движка не получить).
    if engine_alive {
        match model_states().await {
            Ok(st) => {
                let missing: Vec<&str> = st.models.iter()
                    .filter(|m| m.required && !m.installed)
                    .map(|m| m.tag.as_str())
                    .collect();
                let installed = st.models.iter().filter(|m| m.installed).count();
                if missing.is_empty() {
                    out.push(diag("models", "Модели", "ok",
                        format!("Обязательные установлены; всего из набора: {installed}")));
                } else {
                    out.push(diag("models", "Модели", "fail",
                        format!("Не хватает обязательных: {}", missing.join(", "))));
                }
            }
            Err(e) => out.push(diag("models", "Модели", "fail", e.message)),
        }
    } else {
        out.push(diag("models", "Модели", "fail",
            "Движок недоступен — состояние моделей не проверить".into()));
    }

    // 3. База документов: открывается ли, сколько в ней всего. Если рядом лежит
    // резервная копия повреждённого файла — база пересоздавалась: честно предупреждаем
    // (индекс пришлось начать заново, исходные файлы у пользователя целы).
    // SQLite — блокирующий ввод-вывод: уводим из async-воркера в пул потоков.
    let db_check = {
        let app = app.clone();
        tauri::async_runtime::spawn_blocking(move || {
            docstore::db_path(&app).and_then(|p| {
                let conn = docstore::open(&p)?;
                let docs = docstore::list_all_documents(&conn)?;
                Ok((p, docs))
            })
        })
        .await
        .unwrap_or_else(|e| Err(format!("Сбой проверки базы: {e}")))
    };
    match db_check {
        Ok((path, docs)) => {
            let recovered = docstore::corrupt_backup_path(&path).exists();
            let status = if recovered { "warn" } else { "ok" };
            let note = if recovered {
                "; ранее база пересоздавалась после повреждения (копия сохранена рядом)"
            } else {
                ""
            };
            if docs.is_empty() {
                out.push(diag("documents", "База документов", status,
                    format!("Работает, пока пуста (документы добавляются в настройках и на экранах проектов){note}")));
            } else {
                let chunks: i64 = docs.iter().map(|d| d.chunk_count).sum();
                let size_mb = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) as f64
                    / 1024.0 / 1024.0;
                out.push(diag("documents", "База документов", status,
                    format!("Документов: {}, фрагментов: {chunks} ({size_mb:.1} МБ){note}", docs.len())));
            }
        }
        Err(e) => out.push(diag("documents", "База документов", "fail", e)),
    }

    // 4. Место на томе с данными приложения (модели/база/история растут именно там).
    match app.path().app_data_dir() {
        Ok(dir) => {
            let disks = sysinfo::Disks::new_with_refreshed_list();
            // Том, которому принадлежит каталог данных: самая длинная точка
            // монтирования, являющаяся префиксом пути.
            let disk = disks.list().iter()
                .filter(|d| dir.starts_with(d.mount_point()))
                .max_by_key(|d| d.mount_point().as_os_str().len());
            match disk {
                Some(d) => {
                    let free = d.available_space() as f64 / 1024.0 / 1024.0 / 1024.0;
                    let total = d.total_space() as f64 / 1024.0 / 1024.0 / 1024.0;
                    let detail = format!("Свободно {free:.0} ГБ из {total:.0} ГБ");
                    let status = if free < DISK_FAIL_GB { "fail" }
                        else if free < DISK_WARN_GB { "warn" } else { "ok" };
                    out.push(diag("disk", "Место на диске", status, detail));
                }
                None => out.push(diag("disk", "Место на диске", "warn",
                    "Не удалось определить том с данными приложения".into())),
            }
        }
        Err(e) => out.push(diag("disk", "Место на диске", "fail",
            format!("Не удалось получить каталог данных: {e}"))),
    }

    // 5. Железо: тот же детект, что у «светофора» в шапке.
    match detect_hardware() {
        Ok(hw) => {
            let status = match hw.tier.as_str() {
                "green" => "ok",
                "yellow" => "warn",
                _ => "fail",
            };
            let vram = match hw.vram_gb {
                Some(v) => match hw.vram_free_gb {
                    Some(f) => format!("видеопамять {v:.0} ГБ (свободно {f:.1} ГБ)"),
                    None => format!("видеопамять {v:.0} ГБ"),
                },
                None if hw.vram_source == "unified" => "общая память (Apple)".to_string(),
                None => "видеопамять неизвестна".to_string(),
            };
            out.push(diag("hardware", "Железо", status,
                format!("ОЗУ {:.0} ГБ · ядер CPU: {} · {vram}", hw.ram_gb, hw.cpu_cores)));
        }
        Err(e) => out.push(diag("hardware", "Железо", "fail", e)),
    }

    // 6. Хранилище: каталог данных существует и доступен на запись (пробный файл).
    match app.path().app_data_dir() {
        Ok(dir) => {
            let probe = dir.join(".diag-probe.tmp");
            let writable = std::fs::create_dir_all(&dir).is_ok()
                && std::fs::write(&probe, b"ok").is_ok();
            let _ = std::fs::remove_file(&probe);
            if writable {
                out.push(diag("storage", "Хранилище данных", "ok",
                    format!("Доступно на запись: {}", dir.display())));
            } else {
                out.push(diag("storage", "Хранилище данных", "fail",
                    format!("Каталог недоступен на запись: {}", dir.display())));
            }
        }
        Err(e) => out.push(diag("storage", "Хранилище данных", "fail",
            format!("Не удалось получить каталог данных: {e}"))),
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_parsing_and_ordering() {
        assert_eq!(parse_version("0.30.1"), [0, 30, 1]);
        assert_eq!(parse_version("0.17"), [0, 17, 0]); // недостающий компонент = 0
        assert_eq!(parse_version("1.2.3-rc1"), [1, 2, 3]); // нечисловой хвост отброшен
        assert_eq!(parse_version("мусор"), [0, 0, 0]); // экзотика не роняет проверку
        // сравнение с минимумом — обычный порядок массивов
        assert!(parse_version("0.16.9") < [0, 17, 1]);
        assert!(parse_version("0.17.1") >= [0, 17, 1]);
        assert!(parse_version("0.30.0") >= [0, 17, 1]);
    }

    // Кэш VRAM: живой TTL действует одинаково для всех источников; источники без
    // «свободно» (rocm-smi/unified/unknown) после этого живут ещё до VRAM_STATIC_TTL —
    // второе число там в принципе не меняется, перезапускать утилиту незачем.
    #[test]
    fn vram_cache_freshness_ttls() {
        assert!(vram_cache_fresh(Duration::from_secs(1), "nvidia-smi", Some(2.0)));
        assert!(vram_cache_fresh(Duration::from_secs(1), "dxgi", None));
        // источник умеет отдавать «свободно» → после LIVE_TTL обязателен переопрос
        assert!(!vram_cache_fresh(Duration::from_secs(6), "nvidia-smi", Some(2.0)));
        assert!(!vram_cache_fresh(Duration::from_secs(6), "dxgi", None));
        // источник «свободно» не отдаёт в принципе → живём дольше, но не вечно
        assert!(vram_cache_fresh(Duration::from_secs(60), "rocm-smi", None));
        assert!(vram_cache_fresh(Duration::from_secs(60), "unified", None));
        assert!(vram_cache_fresh(Duration::from_secs(60), "unknown", None));
        assert!(!vram_cache_fresh(Duration::from_secs(301), "rocm-smi", None));
    }

    // Таймаут на внешнюю утилиту: зависший процесс не вешает вызывающего — убиваем и
    // честно отдаём None вместо ожидания до конца.
    #[test]
    fn output_with_timeout_kills_hung_process() {
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "sleep 5"]);
        let start = Instant::now();
        let out = output_with_timeout(cmd, Duration::from_millis(150));
        assert!(out.is_none());
        assert!(start.elapsed() < Duration::from_secs(2)); // не ждём зависший процесс до конца
    }

    // Быстрая команда укладывается в таймаут и отдаёт честный вывод.
    #[test]
    fn output_with_timeout_returns_fast_output() {
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "echo hello"]);
        let out = output_with_timeout(cmd, Duration::from_secs(2)).expect("команда должна успеть");
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hello");
    }
}
