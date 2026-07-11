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
// не упирается в скорость USB. Дедуп бесплатный: имена блобов — это sha256, уже
// существующие блобы того же размера не копируются повторно.

use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::ipc::Channel;
use tauri::Manager;

use crate::{engine, PullEvent, PullJob, PullJobGuard, PullOutcome, PullState};

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
                total_gb: bytes as f64 / crate::GB,
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
    let dest = match crate::read_setting_path(&app, "ollama_models_dir") {
        Some(p) => p,
        None => app
            .path()
            .home_dir()
            .map_err(|e| format!("Не удалось определить домашний каталог: {e}"))?
            .join(".ollama")
            .join("models"),
    };

    // Копирование — блокирующий дисковый ввод-вывод на минуты: уводим в отдельный
    // поток, чтобы не занимать асинхронные воркеры (чат продолжает работать).
    let events = on_event.clone();
    let cancel = my_cancel.clone();
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
        import_core(&src, &dest, &cancel, &mut progress)
    })
    .await
    .map_err(|e| format!("Импорт прерван: {e}"))??;

    match result {
        None => Ok(PullOutcome::Cancelled),
        Some(stats) => {
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

/// Ядро импорта (синхронное, тестируемое): скопировать отсутствующие блобы, затем
/// дерево манифестов. None = отменено пользователем (частичный файл убран; уже
/// скопированные блобы валидны — повтор продолжит с места благодаря дедупу).
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

    // План: какие блобы отсутствуют (или битые по размеру) в назначении.
    let mut queue: Vec<(PathBuf, PathBuf, u64)> = Vec::new(); // (src, dest, размер)
    let mut skipped = 0usize;
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
        let target = dest_blobs.join(&name);
        // Дедуп: имя блоба — sha256 содержимого; совпал размер → он уже есть.
        let exists_same = std::fs::metadata(&target)
            .map(|t| t.is_file() && t.len() == md.len())
            .unwrap_or(false);
        if exists_same {
            skipped += 1;
        } else {
            queue.push((p, target, md.len()));
        }
    }

    let total: u64 = queue.iter().map(|(_, _, s)| *s).sum();
    let mut copied: u64 = 0;
    progress(0, total, "Копирование моделей", true);

    for (from, to, _size) in &queue {
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        if !copy_file_chunked(from, to, cancel, total, &mut copied, progress)? {
            return Ok(None); // отменено посреди файла — частичный .part уже убран
        }
    }

    // Манифесты — маленькие файлы, копируем деревом (слияние с существующими).
    if cancel.load(Ordering::Relaxed) {
        return Ok(None);
    }
    let manifests = copy_tree(&src.join("manifests"), &dest_manifests)?;
    progress(total, total, "Готово", true);

    Ok(Some(ImportStats {
        copied_blobs: queue.len(),
        skipped_blobs: skipped,
        manifests,
        copied_bytes: copied,
    }))
}

/// Копия одного блоба порциями во временный файл `.part` с переименованием в конце:
/// оборванная копия никогда не выглядит готовым блобом (дедуп по размеру не обманется).
/// false = отменено (частичный файл удалён).
fn copy_file_chunked(
    from: &Path,
    to: &Path,
    cancel: &AtomicBool,
    total: u64,
    copied: &mut u64,
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
        *copied += n as u64;
        progress(*copied, total, "Копирование моделей", false);
    }
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
    let sizes = crate::installed_sizes().await;
    let kv_bytes = if engine::was_started_by_us(&engine_state) {
        crate::KV_BYTES_Q8
    } else {
        crate::KV_BYTES_F16
    };
    let (vram_gb, _vram_free, _) = crate::detect_vram();
    let available_gb = crate::usable_ram_gb();

    Ok(crate::MODEL_SET
        .iter()
        .map(|spec| {
            let installed_size = sizes.get(spec.tag).copied();
            let weight_gb = installed_size
                .map(|b| b as f64 / crate::GB)
                .unwrap_or(spec.approx_gb);
            // Эвристика KV на полном контексте 8192 (как запасной путь estimate).
            let kv_gb = weight_gb * 0.2 * (kv_bytes / crate::KV_BYTES_Q8);
            let required = weight_gb + kv_gb + crate::COMPUTE_BUF_GB + crate::MEM_SAFETY_GB;
            let (verdict, limiting) = crate::fit_verdict(required, vram_gb, available_gb);
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

    /// Мини-источник: 2 блоба + 1 манифест (структура настоящего каталога Ollama).
    fn build_src(root: &Path) {
        let man = root.join("manifests/registry.ollama.ai/library/test");
        std::fs::create_dir_all(&man).unwrap();
        std::fs::write(man.join("latest"), b"manifest-json").unwrap();
        let blobs = root.join("blobs");
        std::fs::create_dir_all(&blobs).unwrap();
        std::fs::write(blobs.join("sha256-aaa"), vec![b'A'; 100]).unwrap();
        std::fs::write(blobs.join("sha256-bbb"), vec![b'B'; 50]).unwrap();
    }

    #[test]
    fn import_copies_merges_and_dedups() {
        let src = tmp("src");
        build_src(&src);
        let dest = tmp("dest");
        // один блоб «уже установлен» (то же имя и размер) — копироваться не должен
        std::fs::create_dir_all(dest.join("blobs")).unwrap();
        std::fs::write(dest.join("blobs/sha256-aaa"), vec![b'A'; 100]).unwrap();

        let cancel = AtomicBool::new(false);
        let stats = import_core(&src, &dest, &cancel, &mut |_, _, _, _| {})
            .unwrap()
            .expect("не отменялся");
        assert_eq!(stats.copied_blobs, 1, "скопирован только отсутствующий блоб");
        assert_eq!(stats.skipped_blobs, 1, "существующий пропущен (дедуп по sha256)");
        assert_eq!(stats.manifests, 1, "манифест перенесён");
        assert_eq!(stats.copied_bytes, 50);
        assert_eq!(std::fs::read(dest.join("blobs/sha256-bbb")).unwrap().len(), 50);
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
