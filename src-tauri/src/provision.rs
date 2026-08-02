// ── Поставка моделей (флешка/диск): импорт, оценка посильности, поиск носителей ──
//
// Сценарий «установка под ключ»: клиенту привозят флешку с установщиками и
// каталогом моделей Ollama (manifests/ + blobs/). Приложение:
//   • находит носитель с моделями (find_model_sources),
//   • оценивает, какие модели набора машина потянет (assess_models) — ДО установки,
//   • ИМПОРТИРУЕТ выбранное копированием на диск (import_models_from_dir).
//
// Импорт — а не «переключение каталога» (set_models_dir): модели копируются в
// каталог Ollama на компьютере, флешку после импорта можно вынуть, а чтение модели
// не упирается в скорость USB. Целостность бесплатна по дизайну: имя блоба — это
// sha256 его содержимого, поэтому копию сверяем с именем, а уже установленные блобы
// пропускаем только после той же сверки (битая флешка ловится в момент установки,
// а не через неделю невнятной ошибкой загрузки модели).

use serde::Serialize;
use sha2::{Digest, Sha256};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::ipc::Channel;
use tauri::Manager;

use crate::engine;
use crate::models::{PullEvent, PullJob, PullJobGuard, PullOutcome, PullState};

/// Копируем крупными порциями (блоб — один файл на гигабайты), прогресс шлём реже:
/// каждое событие — IPC-сообщение в окно, мельчить незачем.
const COPY_BUF: usize = 8 * 1024 * 1024; // 8 МиБ
const PROGRESS_STEP: u64 = 64 * 1024 * 1024; // событие прогресса раз в 64 МиБ

// ── Поиск носителей с моделями ────────────────────────────────────────────────

/// Найденный источник моделей (обычно флешка): путь + что в нём лежит.
#[derive(Serialize)]
pub(crate) struct ModelSource {
    path: String,
    removable: bool, // съёмный носитель (флешка/внешний диск)
    models: u64,     // сколько тегов (листовых файлов манифестов)
    total_gb: f64,   // суммарный объём блобов
}

/// Где на носителе ищем каталог моделей (соглашение о структуре флешки, см. docs).
const SOURCE_SUBDIRS: [&str; 3] = ["models", "jai/models", "JAI-USB/models"];

/// Обойти смонтированные диски и найти каталоги моделей Ollama. Съёмные носители —
/// первыми (это ожидаемый случай — флешка). Только локальные системные вызовы.
#[tauri::command]
pub(crate) fn find_model_sources() -> Vec<ModelSource> {
    let disks = sysinfo::Disks::new_with_refreshed_list();
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for disk in disks.list() {
        for sub in SOURCE_SUBDIRS {
            let candidate = disk.mount_point().join(sub);
            if !seen.insert(candidate.clone()) {
                continue; // один и тот же mount point может встретиться дважды
            }
            if engine::validate_models_dir(&candidate).is_err() {
                continue; // нет manifests/ + blobs/ — не каталог моделей
            }
            let (models, bytes) = source_stats(&candidate);
            out.push(ModelSource {
                path: candidate.to_string_lossy().into_owned(),
                removable: disk.is_removable(),
                models,
                total_gb: bytes as f64 / crate::memory::GB,
            });
        }
    }
    out.sort_by(|a, b| b.removable.cmp(&a.removable).then(a.path.cmp(&b.path)));
    out
}

/// Сколько в каталоге моделей тегов (листовые файлы манифестов) и байт (блобы).
fn source_stats(dir: &Path) -> (u64, u64) {
    (count_files_recursive(&dir.join("manifests")), blob_bytes(&dir.join("blobs")))
}

/// Рекурсивный подсчёт файлов (дерево манифестов маленькое — это дёшево).
fn count_files_recursive(dir: &Path) -> u64 {
    let mut n = 0;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                n += count_files_recursive(&p);
            } else {
                n += 1;
            }
        }
    }
    n
}

/// Суммарный размер файлов каталога блобов (он плоский: blobs/sha256-…).
fn blob_bytes(dir: &Path) -> u64 {
    let mut total = 0;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            if let Ok(md) = e.metadata() {
                if md.is_file() {
                    total += md.len();
                }
            }
        }
    }
    total
}

// ── Импорт моделей с носителя ─────────────────────────────────────────────────

/// Итог импорта (для теста и финальной надписи).
#[derive(Debug)]
pub(crate) struct ImportStats {
    pub(crate) copied_blobs: usize,
    pub(crate) skipped_blobs: usize,
    pub(crate) manifests: usize,
    pub(crate) copied_bytes: u64,
}

/// Импорт моделей с флешки/диска в каталог Ollama на компьютере, с прогрессом и
/// отменой. Регистрируется в общем PullState: импорт и скачивание взаимоисключаемы,
/// отменяются одной командой cancel_pull, интерфейс блокирует кнопки как обычно.
/// Куда копируем: override `ollama_models_dir` из настроек, иначе стандартный
/// каталог Ollama (~/.ollama/models) — его и наш движок, и системный видят сами.
#[tauri::command]
pub(crate) async fn import_models_from_dir(
    app: tauri::AppHandle,
    path: String,
    on_event: Channel<PullEvent>,
    state: tauri::State<'_, PullState>,
) -> Result<PullOutcome, String> {
    // Регистрация задачи — в синхронной области (std-мьютекс не держим через await).
    let my_cancel = {
        let mut job = state.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(active) = job.as_ref() {
            return Err(format!(
                "Уже идёт установка «{}» — дождитесь её завершения или отмените.",
                active.name
            ));
        }
        let cancel = Arc::new(AtomicBool::new(false));
        *job = Some(PullJob { name: "импорт моделей с диска".into(), cancel: cancel.clone() });
        cancel
    };
    let _guard = PullJobGuard(&state);

    let src = PathBuf::from(path);
    // Отвечает ли на порту ЧУЖОЙ движок (системная служба/ручной запуск) — от этого
    // зависит выбор каталога назначения на Linux.
    let external_engine =
        engine::is_running().await && !engine::was_started_by_us(&app.state::<engine::EngineState>());
    let dest = match crate::settings::read_setting_path(&app, "ollama_models_dir") {
        Some(p) => p,
        None => default_import_dest(&app, external_engine)?,
    };

    // Копирование — блокирующий дисковый ввод-вывод на минуты: уводим в отдельный
    // поток, чтобы не занимать асинхронные воркеры (чат продолжает работать).
    let events = on_event.clone();
    let cancel = my_cancel.clone();
    let (src_job, dest_job) = (src.clone(), dest.clone()); // оригиналы нужны сверке после импорта
    let result = tauri::async_runtime::spawn_blocking(move || {
        let mut last_sent: u64 = 0;
        let mut progress = |done: u64, total: u64, phase: &str, force: bool| {
            if force || done.saturating_sub(last_sent) >= PROGRESS_STEP {
                last_sent = done;
                let _ = events.send(PullEvent::Progress {
                    status: phase.to_string(),
                    completed: done,
                    total,
                });
            }
        };
        import_core(&src_job, &dest_job, &cancel, &mut progress)
    })
    .await
    .map_err(|e| format!("Импорт прерван: {e}"))??;

    match result {
        None => Ok(PullOutcome::Cancelled),
        Some(stats) => {
            // Файлы скопированы — но видит ли их живой движок? При системной
            // установке Ollama (служба под другим пользователем) каталог моделей
            // движка может отличаться от нашего, и без этой сверки импорт
            // рапортовал бы ложный успех, а чат отвечал «model not found».
            if let Some(missing) = missing_in_engine(&src).await {
                return Err(import_invisible_error(&dest, &missing));
            }
            let _ = on_event.send(PullEvent::Progress {
                status: format!(
                    "Импортировано моделей: {} (новых файлов: {}, уже были: {})",
                    stats.manifests, stats.copied_blobs, stats.skipped_blobs
                ),
                completed: stats.copied_bytes,
                total: stats.copied_bytes,
            });
            Ok(PullOutcome::Done)
        }
    }
}

/// Куда импортировать без явной настройки. Обычно ~/.ollama/models — его видят и
/// наш движок, и системный, запущенный от имени пользователя. Особый случай — Linux
/// с работающей Ollama-службой (официальный install.sh): она живёт под отдельным
/// пользователем ollama и читает /usr/share/ollama/.ollama/models. Если порт держит
/// именно внешний движок и его каталог доступен нам на запись — импортируем прямо
/// туда (без sudo и смены прав); недоступен — остаёмся на домашнем, а несовпадение
/// честно поймает сверка с /api/tags после импорта.
fn default_import_dest(app: &tauri::AppHandle, external_engine: bool) -> Result<PathBuf, String> {
    #[cfg(target_os = "linux")]
    if external_engine {
        let system = PathBuf::from("/usr/share/ollama/.ollama/models");
        if system.is_dir() && dir_writable(&system) {
            return Ok(system);
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = external_engine;
    Ok(app
        .path()
        .home_dir()
        .map_err(|e| format!("Не удалось определить домашний каталог: {e}"))?
        .join(".ollama")
        .join("models"))
}

/// Пишется ли каталог от нашего пользователя: пробный файл вместо разбора прав
/// (ACL/группы учитываются сами собой).
#[cfg(target_os = "linux")]
fn dir_writable(dir: &Path) -> bool {
    let probe = dir.join(".jai-write-probe");
    match std::fs::OpenOptions::new().write(true).create_new(true).open(&probe) {
        Ok(f) => {
            drop(f);
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Теги, которых движок НЕ увидел после импорта. None — всё на месте либо сверка
/// невозможна (движок не отвечает — проверка неблокирующая, офлайн-правило проекта).
async fn missing_in_engine(src: &Path) -> Option<Vec<String>> {
    let imported = imported_tags(src);
    if imported.is_empty() {
        return None;
    }
    let resp = crate::HTTP
        .get("http://127.0.0.1:11434/api/tags")
        .timeout(crate::OLLAMA_META_TIMEOUT)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let json: serde_json::Value = resp.json().await.ok()?;
    let installed: std::collections::HashSet<String> = json
        .get("models")
        .and_then(|m| m.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("name").and_then(|n| n.as_str()))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let missing: Vec<String> = imported
        .into_iter()
        .filter(|t| !installed.contains(t))
        .collect();
    (!missing.is_empty()).then_some(missing)
}

/// Теги моделей из дерева манифестов источника — в именах Ollama (`qwen3.5:9b`,
/// `hf.co/…/name:tag`). Путь манифеста: хост/пространство/модель/тег; префикс
/// registry.ollama.ai/library/ движок в /api/tags опускает.
fn imported_tags(src: &Path) -> Vec<String> {
    fn walk(dir: &Path, parts: &mut Vec<String>, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let p = e.path();
            if p.is_dir() {
                parts.push(name);
                walk(&p, parts, out);
                parts.pop();
            } else if !parts.is_empty() {
                let model = parts.join("/");
                let model = model
                    .strip_prefix("registry.ollama.ai/library/")
                    .or_else(|| model.strip_prefix("registry.ollama.ai/"))
                    .unwrap_or(&model);
                out.push(format!("{model}:{name}"));
            }
        }
    }
    let mut out = Vec::new();
    walk(&src.join("manifests"), &mut Vec::new(), &mut out);
    out
}

/// Понятная ошибка «скопировали, но движок не видит»: объясняем причину (другой
/// каталог моделей у движка) и что делать — без терминала.
fn import_invisible_error(dest: &Path, missing: &[String]) -> String {
    format!(
        "Модели скопированы в «{}», но работающий движок Ollama их не видит: \
         он берёт модели из другого каталога. Так бывает, когда Ollama установлена \
         как системная служба со своим каталогом моделей. Что делать: в настройках \
         приложения укажите каталог моделей вашей установки Ollama и повторите \
         импорт — либо остановите системную службу Ollama, чтобы приложение \
         запустило движок само. Движок не увидел: {}",
        dest.display(),
        missing.join(", ")
    )
}

/// Ядро импорта (синхронное, тестируемое): скопировать отсутствующие блобы со сверкой
/// sha256 по имени, затем дерево манифестов. None = отменено пользователем (частичный
/// файл убран; уже скопированные блобы валидны — повтор продолжит с места).
/// Каждый блоб источника читается ровно один раз: либо проверяем уже установленный
/// (хэш сошёлся → пропуск), либо копируем с подсчётом хэша на лету.
pub(crate) fn import_core(
    src: &Path,
    dest: &Path,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64, u64, &str, bool),
) -> Result<Option<ImportStats>, String> {
    engine::validate_models_dir(src)
        .map_err(|e| format!("Это не каталог моделей Ollama: {e}"))?;
    let dest_blobs = dest.join("blobs");
    let dest_manifests = dest.join("manifests");
    std::fs::create_dir_all(&dest_blobs)
        .map_err(|e| format!("Не удалось создать каталог моделей: {e}"))?;
    std::fs::create_dir_all(&dest_manifests)
        .map_err(|e| format!("Не удалось создать каталог моделей: {e}"))?;

    // Опись блобов источника: (путь, имя, размер).
    let mut blobs: Vec<(PathBuf, OsString, u64)> = Vec::new();
    let entries = std::fs::read_dir(src.join("blobs"))
        .map_err(|e| format!("Не удалось прочитать блобы источника: {e}"))?;
    for e in entries.flatten() {
        let p = e.path();
        let md = match e.metadata() {
            Ok(m) if m.is_file() => m,
            _ => continue,
        };
        let name = match p.file_name() {
            Some(n) => n.to_owned(),
            None => continue,
        };
        blobs.push((p, name, md.len()));
    }

    // total — байты, которые предстоит прочитать (проверка или копия). Если проверка
    // существующего блоба провалится и его придётся перекопировать, добавим его
    // размер к total ещё раз — прогресс останется честным.
    let mut total: u64 = blobs.iter().map(|(_, _, s)| *s).sum();
    let mut done: u64 = 0;
    let mut copied_bytes: u64 = 0;
    let mut copied_blobs = 0usize;
    let mut skipped = 0usize;
    progress(0, total, "Копирование моделей", true);

    for (from, name, size) in &blobs {
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let target = dest_blobs.join(name);
        let expected = expected_hash(name);
        let same_size = std::fs::metadata(&target)
            .map(|t| t.is_file() && t.len() == *size)
            .unwrap_or(false);
        if same_size {
            match &expected {
                // Дедуп по хэшу: имя блоба — sha256 содержимого, совпадения размера
                // мало (битая запись прошлого раза, деградировавшая флешка).
                Some(want) => match hash_file(&target, cancel, &mut done, total, progress)? {
                    None => return Ok(None),
                    Some(got) if &got == want => {
                        skipped += 1;
                        continue;
                    }
                    Some(_) => total += size, // битый — перечитаем при копировании
                },
                // Нестандартное имя (хэша в нём нет) — как раньше, дедуп по размеру.
                None => {
                    skipped += 1;
                    done += size;
                    progress(done, total, "Копирование моделей", false);
                    continue;
                }
            }
        }
        if !copy_file_chunked(
            from,
            &target,
            expected.as_deref(),
            cancel,
            &mut done,
            total,
            &mut copied_bytes,
            progress,
        )? {
            return Ok(None); // отменено посреди файла — частичный .part уже убран
        }
        copied_blobs += 1;
    }

    // Манифесты — маленькие файлы, копируем деревом (слияние с существующими).
    if cancel.load(Ordering::Relaxed) {
        return Ok(None);
    }
    let manifests = copy_tree(&src.join("manifests"), &dest_manifests)?;
    progress(total, total, "Готово", true);

    Ok(Some(ImportStats {
        copied_blobs,
        skipped_blobs: skipped,
        manifests,
        copied_bytes,
    }))
}

/// Ожидаемый sha256 из имени блоба Ollama (`sha256-<64 hex>`). None — имя не в этом
/// формате, проверить содержимое нечем.
fn expected_hash(name: &OsStr) -> Option<String> {
    let hex = name.to_str()?.strip_prefix("sha256-")?;
    (hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| hex.to_ascii_lowercase())
}

/// sha256 файла с прогрессом и отменой (None = отменено): проверка уже установленного
/// блоба читает столько же байт, сколько заняла бы копия, — UI не должен «висеть».
fn hash_file(
    path: &Path,
    cancel: &AtomicBool,
    done: &mut u64,
    total: u64,
    progress: &mut dyn FnMut(u64, u64, &str, bool),
) -> Result<Option<String>, String> {
    use std::io::Read;
    let mut reader = std::fs::File::open(path)
        .map_err(|e| format!("Не удалось прочитать «{}»: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; COPY_BUF];
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let n = reader
            .read(&mut buf)
            .map_err(|e| format!("Сбой чтения при проверке блоба: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        *done += n as u64;
        progress(*done, total, "Проверка установленных моделей", false);
    }
    Ok(Some(hex_digest(hasher)))
}

fn hex_digest(hasher: Sha256) -> String {
    hasher.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Копия одного блоба порциями во временный файл `.part` с переименованием в конце:
/// оборванная копия никогда не выглядит готовым блобом. По пути считается sha256 и
/// сверяется с ожидаемым из имени — битый носитель ловим здесь, у клиента без
/// интернета другого шанса перекачать модель нет. false = отменено (частичный файл
/// удалён).
#[allow(clippy::too_many_arguments)]
fn copy_file_chunked(
    from: &Path,
    to: &Path,
    expected: Option<&str>,
    cancel: &AtomicBool,
    done: &mut u64,
    total: u64,
    copied_bytes: &mut u64,
    progress: &mut dyn FnMut(u64, u64, &str, bool),
) -> Result<bool, String> {
    use std::io::{Read, Write};
    let mut part = to.as_os_str().to_os_string();
    part.push(".part");
    let part = PathBuf::from(part);
    let _ = std::fs::remove_file(&part); // остаток прежней оборванной попытки

    let mut reader = std::fs::File::open(from)
        .map_err(|e| format!("Не удалось прочитать «{}»: {e}", from.display()))?;
    let mut writer = std::fs::File::create(&part)
        .map_err(|e| format!("Не удалось записать «{}»: {e}", part.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; COPY_BUF];
    loop {
        if cancel.load(Ordering::Relaxed) {
            drop(writer);
            let _ = std::fs::remove_file(&part);
            return Ok(false);
        }
        let n = reader
            .read(&mut buf)
            .map_err(|e| format!("Сбой чтения с носителя: {e}"))?;
        if n == 0 {
            break;
        }
        writer
            .write_all(&buf[..n])
            .map_err(|e| format!("Сбой записи на диск (хватает ли места?): {e}"))?;
        hasher.update(&buf[..n]);
        *done += n as u64;
        *copied_bytes += n as u64;
        progress(*done, total, "Копирование моделей", false);
    }
    if let Some(want) = expected {
        let got = hex_digest(hasher);
        if got != want {
            drop(writer);
            let _ = std::fs::remove_file(&part);
            return Err(format!(
                "Файл модели на носителе повреждён (контрольная сумма не сходится): «{}». \
                 Перезапишите флешку и повторите импорт.",
                from.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
            ));
        }
    }
    // fsync до rename: после сбоя питания под финальным именем не должен оказаться
    // недописанный файл — иначе он навсегда прошёл бы дедуп как валидный.
    writer
        .sync_all()
        .map_err(|e| format!("Не удалось сохранить блоб на диск: {e}"))?;
    drop(writer);
    let _ = std::fs::remove_file(to); // Windows: rename поверх существующего не работает
    std::fs::rename(&part, to).map_err(|e| format!("Не удалось сохранить блоб: {e}"))?;
    Ok(true)
}

/// Рекурсивная копия дерева (манифесты): создаёт подкаталоги, файлы перезаписывает
/// (слияние с уже установленными моделями). Возвращает число скопированных файлов.
fn copy_tree(src: &Path, dest: &Path) -> Result<usize, String> {
    let mut copied = 0;
    let entries = std::fs::read_dir(src)
        .map_err(|e| format!("Не удалось прочитать манифесты источника: {e}"))?;
    for e in entries.flatten() {
        let from = e.path();
        let to = dest.join(e.file_name());
        if from.is_dir() {
            std::fs::create_dir_all(&to)
                .map_err(|e| format!("Не удалось создать каталог манифестов: {e}"))?;
            copied += copy_tree(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)
                .map_err(|e| format!("Не удалось скопировать манифест: {e}"))?;
            copied += 1;
        }
    }
    Ok(copied)
}

// ── Оценка посильности набора моделей (ДО установки) ──────────────────────────

/// Оценка одной модели набора для мастера установки/каталога.
#[derive(Serialize)]
pub(crate) struct ModelAssessment {
    tag: String,
    role: String,
    title: String,
    required: bool,
    installed: bool,
    size_gb: f64,     // фактический размер (установлена) либо приблизительный
    approx: bool,     // размер приблизительный (модель ещё не установлена)
    verdict: String,  // "ok" | "tight" | "no" — потянет ли машина
    limiting: String, // "ram" | "vram" — что ограничивает
}

/// Оценить ВЕСЬ набор моделей против текущего железа — до установки. Для
/// установленных берётся точный размер из /api/tags (движок молчит — не страшно,
/// остаёмся на приблизительных весах). KV — той же эвристикой, что запасной путь
/// estimate(): точных метаданных у неустановленной модели нет.
#[tauri::command]
pub(crate) async fn assess_models(
    engine_state: tauri::State<'_, engine::EngineState>,
) -> Result<Vec<ModelAssessment>, String> {
    let sizes = crate::models::installed_sizes().await;
    let kv_bytes = if engine::was_started_by_us(&engine_state) {
        crate::memory::KV_BYTES_Q8
    } else {
        crate::memory::KV_BYTES_F16
    };
    let (vram_gb, _vram_free, _) = crate::diagnostics::detect_vram();
    let available_gb = crate::memory::usable_ram_gb();

    Ok(crate::models::MODEL_SET
        .iter()
        .map(|spec| {
            let installed_size = sizes.get(spec.tag).copied();
            let weight_gb = installed_size
                .map(|b| b as f64 / crate::memory::GB)
                .unwrap_or(spec.approx_gb);
            // Эвристика KV на полном контексте 8192 (как запасной путь estimate).
            let kv_gb = weight_gb * 0.2 * (kv_bytes / crate::memory::KV_BYTES_Q8);
            let required = weight_gb + kv_gb + crate::memory::COMPUTE_BUF_GB + crate::memory::MEM_SAFETY_GB;
            let (verdict, limiting) = crate::memory::fit_verdict(required, vram_gb, available_gb);
            ModelAssessment {
                tag: spec.tag.to_string(),
                role: spec.role.to_string(),
                title: spec.title.to_string(),
                required: spec.required,
                installed: installed_size.is_some(),
                size_gb: weight_gb,
                approx: installed_size.is_none(),
                verdict: verdict.to_string(),
                limiting: limiting.to_string(),
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Отдельный чистый каталог для теста (имя — чтобы тесты не пересекались).
    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("jai-prov-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Содержимое блобов мини-источника (имена по-настоящему хэшируются).
    const BLOB_A: &[u8] = &[b'A'; 100];
    const BLOB_B: &[u8] = &[b'B'; 50];

    /// Имя блоба Ollama = sha256 содержимого — тесты используют настоящие имена,
    /// иначе сверка при копировании честно их отвергнет.
    fn blob_name(data: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(data);
        format!("sha256-{}", hex_digest(h))
    }

    /// Мини-источник: 2 блоба + 1 манифест (структура настоящего каталога Ollama).
    fn build_src(root: &Path) {
        let man = root.join("manifests/registry.ollama.ai/library/test");
        std::fs::create_dir_all(&man).unwrap();
        std::fs::write(man.join("latest"), b"manifest-json").unwrap();
        let blobs = root.join("blobs");
        std::fs::create_dir_all(&blobs).unwrap();
        std::fs::write(blobs.join(blob_name(BLOB_A)), BLOB_A).unwrap();
        std::fs::write(blobs.join(blob_name(BLOB_B)), BLOB_B).unwrap();
    }

    #[test]
    fn import_copies_merges_and_dedups() {
        let src = tmp("src");
        build_src(&src);
        let dest = tmp("dest");
        // один блоб «уже установлен» (имя и содержимое сходятся) — копироваться не должен
        std::fs::create_dir_all(dest.join("blobs")).unwrap();
        std::fs::write(dest.join("blobs").join(blob_name(BLOB_A)), BLOB_A).unwrap();

        let cancel = AtomicBool::new(false);
        let stats = import_core(&src, &dest, &cancel, &mut |_, _, _, _| {})
            .unwrap()
            .expect("не отменялся");
        assert_eq!(stats.copied_blobs, 1, "скопирован только отсутствующий блоб");
        assert_eq!(stats.skipped_blobs, 1, "существующий пропущен (дедуп по sha256)");
        assert_eq!(stats.manifests, 1, "манифест перенесён");
        assert_eq!(stats.copied_bytes, 50);
        assert_eq!(
            std::fs::read(dest.join("blobs").join(blob_name(BLOB_B))).unwrap().len(),
            50
        );
        assert!(dest
            .join("manifests/registry.ollama.ai/library/test/latest")
            .is_file());
        // повторный импорт полностью идемпотентен
        let again = import_core(&src, &dest, &cancel, &mut |_, _, _, _| {})
            .unwrap()
            .unwrap();
        assert_eq!(again.copied_blobs, 0);
        assert_eq!(again.skipped_blobs, 2);
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dest);
    }

    // Существующий блоб с верным именем и размером, но битым содержимым (сбой
    // прошлой записи) обнаруживается сверкой хэша и перезаписывается.
    #[test]
    fn import_replaces_corrupt_existing_blob() {
        let src = tmp("fixsrc");
        build_src(&src);
        let dest = tmp("fixdest");
        std::fs::create_dir_all(dest.join("blobs")).unwrap();
        std::fs::write(dest.join("blobs").join(blob_name(BLOB_A)), vec![b'X'; 100]).unwrap();

        let cancel = AtomicBool::new(false);
        let stats = import_core(&src, &dest, &cancel, &mut |_, _, _, _| {})
            .unwrap()
            .unwrap();
        assert_eq!(stats.copied_blobs, 2, "битый блоб перекопирован, не пропущен");
        assert_eq!(stats.skipped_blobs, 0);
        assert_eq!(
            std::fs::read(dest.join("blobs").join(blob_name(BLOB_A))).unwrap(),
            BLOB_A,
            "содержимое восстановлено с носителя"
        );
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dest);
    }

    // Битый блоб на носителе (содержимое не сходится с именем) — понятная ошибка,
    // под финальным именем в назначении ничего не появляется.
    #[test]
    fn import_rejects_corrupt_source_blob() {
        let src = tmp("corsrc");
        let dest = tmp("cordest");
        let man = src.join("manifests/registry.ollama.ai/library/test");
        std::fs::create_dir_all(&man).unwrap();
        std::fs::write(man.join("latest"), b"manifest-json").unwrap();
        std::fs::create_dir_all(src.join("blobs")).unwrap();
        let name = blob_name(BLOB_A); // имя честное, содержимое — нет
        std::fs::write(src.join("blobs").join(&name), vec![b'X'; 100]).unwrap();

        let cancel = AtomicBool::new(false);
        let err = import_core(&src, &dest, &cancel, &mut |_, _, _, _| {}).unwrap_err();
        assert!(err.contains("повреждён"), "ошибка объясняет причину: {err}");
        assert!(
            !dest.join("blobs").join(&name).exists(),
            "битый блоб не опубликован под финальным именем"
        );
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dest);
    }

    // Имена движка из дерева манифестов: library-префикс опускается, hf.co — нет.
    #[test]
    fn imported_tags_match_engine_names() {
        let src = tmp("tags");
        for (dir, tag) in [
            ("manifests/registry.ollama.ai/library/qwen3.5", "9b"),
            ("manifests/hf.co/t-tech/T-lite-it-2.1-GGUF", "Q4_K_M"),
        ] {
            let d = src.join(dir);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join(tag), b"{}").unwrap();
        }
        let mut tags = imported_tags(&src);
        tags.sort();
        assert_eq!(
            tags,
            vec![
                "hf.co/t-tech/T-lite-it-2.1-GGUF:Q4_K_M".to_string(),
                "qwen3.5:9b".to_string()
            ]
        );
        let _ = std::fs::remove_dir_all(&src);
    }

    #[test]
    fn import_cancelled_leaves_no_partials() {
        let src = tmp("csrc");
        build_src(&src);
        let dest = tmp("cdest");
        let cancel = AtomicBool::new(true); // отменено сразу
        let out = import_core(&src, &dest, &cancel, &mut |_, _, _, _| {}).unwrap();
        assert!(out.is_none(), "отмена → None");
        // ни блобов, ни частичных .part в назначении
        let leftovers = std::fs::read_dir(dest.join("blobs"))
            .map(|d| d.count())
            .unwrap_or(0);
        assert_eq!(leftovers, 0, "после отмены нет мусора");
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dest);
    }

    #[test]
    fn import_rejects_non_model_dir() {
        let src = tmp("bad");
        let dest = tmp("baddest");
        let cancel = AtomicBool::new(false);
        assert!(import_core(&src, &dest, &cancel, &mut |_, _, _, _| {}).is_err());
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dest);
    }

    #[test]
    fn source_stats_counts_tags_and_bytes() {
        let src = tmp("stats");
        build_src(&src);
        let (models, bytes) = source_stats(&src);
        assert_eq!(models, 1);
        assert_eq!(bytes, 150);
        let _ = std::fs::remove_dir_all(&src);
    }
}
