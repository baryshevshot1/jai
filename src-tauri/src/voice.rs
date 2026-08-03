// ── Голосовой ввод: запись с микрофона и распознавание речи ──────────────────
// Пользователь зажимает кнопку микрофона, говорит, отпускает — текст попадает в поле
// ввода. Всё локально: звук не покидает компьютер, как и остальные данные (152-ФЗ).
//
// Почему запись именно здесь, в Rust, а не через getUserMedia в окне: на целевом
// Linux webview (WebKitGTK) доступ к микрофону ненадёжен, а звук нужен одинаково на
// всех трёх системах. cpal даёт один путь на Linux (ALSA), Windows (WASAPI) и macOS.
//
// Почему whisper.cpp, а не sherpa-onnx с более точной русской моделью: whisper-rs
// линкуется СТАТИЧЕСКИ и ничего не скачивает при сборке, а sherpa кладёт рядом с
// приложением динамические библиотеки и требует настройки путей загрузки на каждой
// системе. Для коробочной поставки «поставили и уехали» отказ вида «библиотека не
// найдена» дороже, чем несколько процентов точности.
//
// ПАМЯТЬ. Распознаватель — второй едок памяти рядом с языковой моделью, а жалоба
// заказчика была именно на переполнение. Поэтому: считаем СТРОГО на процессоре (ни
// байта видеопамяти), модель грузим ЛЕНИВО при первом использовании и выгружаем по
// простою, а на время распознавания запись и генерация не идут одновременно.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
// Mutex и Instant нужны только реализации с распознаванием: без фичи voice модуль
// сводится к честным отказам, и лишнее не должно висеть мёртвым кодом. Duration же
// нужен и фоновому присмотру за памятью, который есть в обеих сборках.
#[cfg(feature = "voice")]
use std::sync::Mutex;
use std::time::Duration;
#[cfg(feature = "voice")]
use std::time::Instant;

use sha2::{Digest, Sha256};
use tauri::ipc::Channel;

#[cfg(feature = "voice")]
use crate::error::ErrorCode;
use crate::error::{AppError, AppResult};
use crate::models::{PullEvent, PullJob, PullJobGuard, PullOutcome, PullState};

/// Частота, которую ждёт whisper. Микрофоны отдают 44.1/48 кГц — пересчитываем.
#[cfg(feature = "voice")]
const TARGET_HZ: u32 = 16_000;

/// Потолок длительности одной диктовки. Защита от «зажал и забыл»: без неё буфер
/// растёт бесконечно (16 кГц × 4 байта ≈ 3.8 МБ в минуту), а распознавание очень
/// длинной записи занимает минуты и выглядит как зависание.
#[cfg(feature = "voice")]
const MAX_RECORDING_SECS: usize = 120;

/// Сколько распознаватель живёт в памяти без дела. Симметрично keep_alive языковой
/// модели: подряд надиктованные фразы не платят за повторную загрузку, а забытый
/// микрофон не держит сотни мегабайт вечно.
#[cfg(feature = "voice")]
const MODEL_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Имя файла модели в каталоге данных приложения. Кладётся мастером установки
/// (с флешки) или скачивается пользователем — путь один и тот же.
pub(crate) const MODEL_FILE: &str = "ggml-small.bin";

/// Где лежит модель распознавания: appDataDir/stt/<файл>. НЕ в каталоге Ollama —
/// это не её модель, у неё другой формат хранения (manifests/blobs).
pub(crate) fn model_path(app: &tauri::AppHandle) -> AppResult<std::path::PathBuf> {
    use tauri::Manager;
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| AppError::unknown(format!("Не удалось определить каталог данных: {e}")))?;
    Ok(dir.join("stt").join(MODEL_FILE))
}

/// Готов ли голосовой ввод: установлена ли модель распознавания.
/// Мягкая проверка для интерфейса — кнопку микрофона показываем только когда есть чем
/// распознавать, а не даём нажать и получить ошибку.
#[tauri::command]
pub(crate) async fn voice_available(app: tauri::AppHandle) -> bool {
    model_path(&app).map(|p| p.is_file()).unwrap_or(false)
}

// ── Поставка модели распознавания: скачивание из интернета ───────────────────
// Модель ставится один раз и двумя путями: с флешки (основной сценарий поставки
// «под ключ» — provision::import_voice_model) и из интернета, если он есть. Оба
// пути ведут в один файл (model_path) и оба заканчиваются сверкой sha256: битую
// модель честнее отвергнуть сразу, чем показать невнятный сбой при первой диктовке
// (у клиента без интернета перекачать её будет нечем).

/// Официальный источник модели — репозиторий whisper.cpp на Hugging Face. Ссылка
/// resolve/main отдаёт перенаправление на CDN раздачи; переходы разрешает и
/// проверяет изолированный клиент онлайн-слоя, и только по https.
pub(crate) const MODEL_URL: &str =
    "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.bin";

/// sha256 файла ggml-small.bin (он же oid LFS в Hugging Face). Одно значение на оба
/// пути установки — и для скачанного файла, и для копии с носителя.
pub(crate) const MODEL_SHA256: &str =
    "1be3a9b2063867b937e64e2ec7483364a79917e157fa98c5d94b5c1fffea987b";

/// Размер файла, байт. Нужен, когда сервер не сообщает длину ответа: без него
/// прогресс не показать, а пользователь ждёт почти полгигабайта. Он же отсекает
/// подсунутый по ошибке чужой файл ещё до копирования с флешки.
pub(crate) const MODEL_BYTES: u64 = 487_601_967;

/// Запас свободного места сверх самого файла: файловой системе нужно куда-то
/// положить и `.part`, и переименованный результат, и собственные метаданные.
const SPACE_MARGIN: u64 = 64 * 1024 * 1024;

/// Временное имя загрузки: оборванная закачка не должна выглядеть готовой моделью.
fn part_path(target: &Path) -> PathBuf {
    let mut p = target.as_os_str().to_os_string();
    p.push(".part");
    PathBuf::from(p)
}

/// Свободное место на том диске, куда ляжет файл. None — определить не удалось;
/// это не повод отказывать в установке, только повод не проверять.
fn free_space_for(path: &Path) -> Option<u64> {
    let disks = sysinfo::Disks::new_with_refreshed_list();
    // Точек монтирования, являющихся префиксом пути, может быть несколько
    // (`/` и `/Users` на macOS) — нужна самая длинная, она и есть наш диск.
    disks
        .list()
        .iter()
        .filter(|d| path.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len())
        .map(|d| d.available_space())
}

/// Что видно интерфейсу про поставку модели: установлена ли, и если нет — сколько
/// уже скачано. Без второго числа карточка не может честно предложить «Продолжить»
/// вместо «Установить» и выглядела бы так, будто прогресс потерян.
#[derive(serde::Serialize)]
pub(crate) struct VoiceModelState {
    installed: bool,
    partial_bytes: u64,
    total_bytes: u64,
}

#[tauri::command]
pub(crate) async fn voice_model_state(app: tauri::AppHandle) -> VoiceModelState {
    let (installed, partial_bytes) = match model_path(&app) {
        Ok(target) => (target.is_file(), crate::download::part_len(&part_path(&target))),
        Err(_) => (false, 0),
    };
    VoiceModelState {
        installed,
        // Установленная модель не «частично скачана»: остаток .part рядом с готовым
        // файлом — мусор прошлой попытки, и предлагать его докачать бессмысленно.
        partial_bytes: if installed { 0 } else { partial_bytes.min(MODEL_BYTES) },
        total_bytes: MODEL_BYTES,
    }
}

/// Опубликовать файл под финальным именем — ТОЛЬКО если контрольная сумма сошлась.
/// Не сошлась: файл удаляем и говорим человеку, что делать. Модель, не прошедшая
/// сверку, до распознавания не доходит вовсе.
fn publish_verified(part: &Path, target: &Path, got: &str) -> AppResult<()> {
    if got != MODEL_SHA256 {
        let _ = std::fs::remove_file(part);
        // Битую загрузку убираем сразу — значит, и кнопки «Продолжить» после неё не
        // будет. Поэтому в тексте зовём «Установить», а не «Начать заново»: указывать
        // человеку на кнопку, которой в этот момент нет, — верный способ его запутать.
        return Err(AppError::unknown(
            "Файл модели распознавания речи получен с ошибкой (контрольная сумма не \
             сходится) — скачанное удалено. Нажмите «Установить», чтобы попробовать \
             ещё раз, или поставьте модель с флешки.",
        ));
    }
    let _ = std::fs::remove_file(target); // Windows: rename поверх существующего не работает
    std::fs::rename(part, target)
        .map_err(|e| AppError::unknown(format!("Не удалось сохранить модель: {e}")))?;
    Ok(())
}

/// Посчитать sha256 готового файла потоковым чтением.
///
/// Почему не «на лету» при скачивании, как было раньше: хэшер нельзя продолжить
/// после перезапуска приложения, а докачка обязана переживать и перезапуск тоже.
/// Чтение 465 МБ с диска — секунды, и это честная цена за то, что прогресс не
/// теряется. None — отменено пользователем.
fn digest_file(
    path: &Path,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64),
) -> Result<Option<String>, String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)
        .map_err(|e| format!("Не удалось прочитать загруженный файл: {e}"))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    let mut done: u64 = 0;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let n = f.read(&mut buf).map_err(|e| format!("Сбой чтения при проверке: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        done += n as u64;
        progress(done);
    }
    Ok(Some(crate::provision::hex_digest(hasher)))
}

/// Скачать модель распознавания речи (около 470 МБ) с докачкой, повторами,
/// прогрессом и отменой. `restart = true` — начать с нуля, выбросив недокачанное.
///
/// Регистрируется в общем PullState: установка моделей движка и модели распознавания
/// взаимоисключаемы и отменяются одной командой cancel_pull. Наружу идём отдельным
/// клиентом БОЛЬШИХ загрузок (tools::download_client) — у него нет общего таймаута на
/// запрос, который прежде убивал эту загрузку на двадцатой секунде всегда и у всех.
/// Сам ход загрузки — в crate::download, где он покрыт тестами на локальном сервере.
#[tauri::command]
pub(crate) async fn voice_model_download(
    app: tauri::AppHandle,
    restart: bool,
    on_event: Channel<PullEvent>,
    state: tauri::State<'_, PullState>,
) -> AppResult<PullOutcome> {
    // Регистрация задачи — в синхронной области (std-мьютекс не держим через await);
    // гард ниже снимет её при любом выходе из функции.
    let my_cancel = {
        let mut job = state.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(active) = job.as_ref() {
            return Err(AppError::unknown(format!(
                "Уже идёт установка «{}» — дождитесь её завершения или отмените.",
                active.name
            )));
        }
        let cancel = Arc::new(AtomicBool::new(false));
        *job = Some(PullJob {
            name: "модель распознавания речи".into(),
            cancel: cancel.clone(),
        });
        cancel
    };
    let _guard = PullJobGuard(&state);

    let target = model_path(&app)?;
    if let Some(dir) = target.parent() {
        std::fs::create_dir_all(dir).map_err(|e| {
            AppError::unknown(format!("Не удалось создать каталог для модели: {e}"))
        })?;
    }
    let part = part_path(&target);
    if restart {
        let _ = std::fs::remove_file(&part);
    }

    // Место проверяем ДО загрузки: узнать о нехватке после получаса ожидания —
    // худший момент из возможных.
    let have = crate::download::part_len(&part);
    let need = MODEL_BYTES.saturating_sub(have) + SPACE_MARGIN;
    if let Some(free) = free_space_for(&target) {
        if free < need {
            return Err(AppError::unknown(format!(
                "Не хватает места на диске: нужно ещё {}, свободно {}. Освободите место \
                 и повторите.",
                crate::download::mb(need),
                crate::download::mb(free)
            )));
        }
    }

    crate::journal::info(
        "голос",
        format!("загрузка модели: уже есть {have} из {MODEL_BYTES} байт, restart={restart}"),
    );

    let events = on_event.clone();
    let plan = crate::download::Plan {
        url: MODEL_URL,
        part: &part,
        total: MODEL_BYTES,
        label: "Скачивание модели распознавания речи",
        retry_pauses: &crate::download::RETRY_PAUSES,
    };
    let client = crate::tools::download_client()?;
    let fetched = crate::download::resumable(&client, &plan, &my_cancel, &move |status, done, total| {
        let _ = events.send(PullEvent::Progress {
            status: status.to_string(),
            completed: done,
            total,
        });
    })
    .await?;
    if let crate::download::Outcome::Cancelled = fetched {
        return Ok(PullOutcome::Cancelled);
    }

    // ── Проверка целостности ────────────────────────────────────────────────
    // Только после неё файл получает имя модели. Неполная или битая загрузка не
    // должна выглядеть установленной: у клиента без интернета перекачать её нечем,
    // а сбой всплыл бы при первой диктовке и выглядел бы как поломка приложения.
    let _ = on_event.send(PullEvent::Progress {
        status: "Проверка целостности".to_string(),
        completed: 0,
        total: MODEL_BYTES,
    });
    let part_for_hash = part.clone();
    let cancel_for_hash = my_cancel.clone();
    let events = on_event.clone();
    let digest = tauri::async_runtime::spawn_blocking(move || {
        let mut last = 0u64;
        digest_file(&part_for_hash, &cancel_for_hash, &mut |done| {
            if done - last >= crate::download::PROGRESS_STEP {
                last = done;
                let _ = events.send(PullEvent::Progress {
                    status: "Проверка целостности".to_string(),
                    completed: done,
                    total: MODEL_BYTES,
                });
            }
        })
    })
    .await
    .map_err(|e| AppError::unknown(format!("Проверка прервана: {e}")))??;

    let Some(digest) = digest else {
        return Ok(PullOutcome::Cancelled);
    };
    publish_verified(&part, &target, &digest)?;
    crate::journal::info("голос", "модель распознавания установлена, сумма сошлась");
    let _ = on_event.send(PullEvent::Progress {
        status: "Готово".to_string(),
        completed: MODEL_BYTES,
        total: MODEL_BYTES,
    });
    Ok(PullOutcome::Done)
}

#[cfg(feature = "voice")]
/// Пик и среднеквадратичный уровень сигнала. Оба нужны: короткий щелчок даёт
/// большой пик при нулевом СКЗ, а ровный фоновый шум — наоборот.
pub(crate) fn level(audio: &[f32]) -> (f32, f32) {
    let mut peak = 0.0f32;
    let mut sum = 0.0f64;
    for s in audio {
        peak = peak.max(s.abs());
        sum += (*s as f64) * (*s as f64);
    }
    let rms = (sum / audio.len().max(1) as f64).sqrt() as f32;
    (peak, rms)
}

#[cfg(feature = "voice")]
/// Порог «в записи ничего нет».
///
/// Зачем вообще проверять. Whisper на тишине НЕ молчит — он выдумывает текст,
/// обычно фразы из субтитровых корпусов, на которых учился («Редактор субтитров
/// …», «Продолжение следует…»). Пользователь при этом видит не «микрофон не
/// работает», а осмысленную чужую фразу в ответ на любой свой вопрос — и идёт
/// чинить не то. Ровно так и проявился отказ доступа к микрофону на macOS.
///
/// Порог намеренно очень низкий (−46 dBFS по пику): задача — отличить «сигнала
/// нет вовсе» от «говорили тихо», а не судить о качестве записи. Всё, что громче,
/// уходит в распознавание как есть.
pub(crate) fn is_silence(peak: f32, rms: f32) -> bool {
    peak < 0.005 || rms < 0.0005
}

// ── Ниже — реализация, требующая собранного распознавателя ───────────────────
// Без фичи voice команды отвечают честным отказом: приложение остаётся рабочим,
// просто без голоса (так собирают быстрые проверки, где cmake ни к чему).

#[cfg(not(feature = "voice"))]
mod imp {
    use super::*;

    pub(super) fn start(_app: &tauri::AppHandle) -> AppResult<()> {
        Err(AppError::unknown("Голосовой ввод не собран в этой версии приложения."))
    }
    pub(super) fn stop(_app: &tauri::AppHandle) -> AppResult<String> {
        Err(AppError::unknown("Голосовой ввод не собран в этой версии приложения."))
    }
    /// Отмена без записи ничего не делает и НЕ считается ошибкой: интерфейс зовёт её
    /// «на всякий случай» (ушли из окна, нажали Esc), и получать за это отказ незачем.
    pub(super) fn cancel() {}
    pub(super) fn release_idle() {}
}

#[cfg(feature = "voice")]
mod imp {
    use super::*;
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    /// Идущая запись. Сам поток устройства СЮДА НЕ ПОПАДАЕТ: он живёт в
    /// собственном потоке ОС и там же уничтожается.
    ///
    /// Почему так, а не «положить поток в статик»: команды voice_start и voice_stop
    /// исполняются в пуле потоков, то есть в РАЗНЫХ потоках. Звуковой поток
    /// (CoreAudio на macOS, WASAPI на Windows) привязан к создавшему его потоку, и
    /// уничтожение из чужого — неопределённое поведение, а не просто нечистоплотность.
    /// Поэтому владение остаётся у отдельного потока, а наружу торчат только каналы.
    struct Recording {
        stop_tx: std::sync::mpsc::Sender<()>,
        done_rx: std::sync::mpsc::Receiver<Captured>,
    }

    /// Что записалось, вместе с параметрами устройства (нужны для пересчёта частоты).
    struct Captured {
        samples: Vec<f32>,
        input_hz: u32,
        channels: u16,
    }

    static RECORDING: Mutex<Option<Recording>> = Mutex::new(None);
    static MODEL: Mutex<Option<LoadedModel>> = Mutex::new(None);

    struct LoadedModel {
        ctx: whisper_rs::WhisperContext,
        last_used: Instant,
    }

    pub(super) fn start(_app: &tauri::AppHandle) -> AppResult<()> {
        let mut slot = RECORDING.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_some() {
            return Err(AppError::unknown("Запись уже идёт."));
        }
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<Captured>();
        // Отдельным каналом сообщаем, удалось ли ОТКРЫТЬ микрофон: пользователю нужно
        // сразу понимать, что записи не будет, а не узнавать это при отпускании кнопки.
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();

        std::thread::spawn(move || {
            let opened = open_stream();
            let (stream, input_hz, channels, samples) = match opened {
                Ok(v) => {
                    let _ = ready_tx.send(Ok(()));
                    v
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            // Ждём команды «стоп». Обрыв канала (паника вызывающего) тоже завершает
            // поток — микрофон не остаётся открытым навсегда.
            let _ = stop_rx.recv();
            drop(stream); // уничтожается ЗДЕСЬ же, где создан
            let samples = std::mem::take(&mut *samples.lock().unwrap_or_else(|e| e.into_inner()));
            let _ = done_tx.send(Captured { samples, input_hz, channels });
        });

        match ready_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(AppError::unknown(e)),
            Err(_) => return Err(AppError::unknown("Не удалось запустить запись.")),
        }
        *slot = Some(Recording { stop_tx, done_rx });
        Ok(())
    }

    /// Открыть микрофон и начать накопление отсчётов. Вызывается ТОЛЬКО из потока,
    /// который потом сам же уничтожит поток устройства.
    #[allow(clippy::type_complexity)]
    fn open_stream(
    ) -> Result<(cpal::Stream, u32, u16, std::sync::Arc<Mutex<Vec<f32>>>), String> {
        let host = cpal::default_host();
        let device = host.default_input_device().ok_or_else(|| {
            "Микрофон не найден. Проверьте, что устройство записи подключено и \
             разрешено в настройках системы."
                .to_string()
        })?;
        let cfg = device
            .default_input_config()
            .map_err(|e| format!("Не удалось получить настройки микрофона: {e}"))?;
        let input_hz = cfg.sample_rate();
        let channels = cfg.channels();
        let samples = std::sync::Arc::new(Mutex::new(Vec::<f32>::new()));

        // Потолок буфера считаем в отсчётах ИСХОДНОЙ частоты: обрываем накопление,
        // а не саму запись — пользователь просто получит первые две минуты.
        let cap = input_hz as usize * channels as usize * MAX_RECORDING_SECS;
        let sink = samples.clone();
        let err_fn = |e| eprintln!("ошибка потока записи: {e}");

        let stream = match cfg.sample_format() {
            cpal::SampleFormat::F32 => device.build_input_stream(
                cfg.into(),
                move |data: &[f32], _: &_| push(&sink, data, cap),
                err_fn,
                None,
            ),
            cpal::SampleFormat::I16 => device.build_input_stream(
                cfg.into(),
                move |data: &[i16], _: &_| {
                    let f: Vec<f32> = data.iter().map(|s| *s as f32 / i16::MAX as f32).collect();
                    push(&sink, &f, cap)
                },
                err_fn,
                None,
            ),
            cpal::SampleFormat::U16 => device.build_input_stream(
                cfg.into(),
                move |data: &[u16], _: &_| {
                    let f: Vec<f32> = data
                        .iter()
                        .map(|s| (*s as f32 - u16::MAX as f32 / 2.0) / (u16::MAX as f32 / 2.0))
                        .collect();
                    push(&sink, &f, cap)
                },
                err_fn,
                None,
            ),
            other => {
                return Err(format!("Микрофон отдаёт неподдерживаемый формат звука ({other:?})."))
            }
        }
        .map_err(|e| format!("Не удалось начать запись: {e}"))?;

        stream.play().map_err(|e| format!("Не удалось запустить микрофон: {e}"))?;
        Ok((stream, input_hz, channels, samples))
    }

    fn push(sink: &std::sync::Arc<Mutex<Vec<f32>>>, data: &[f32], cap: usize) {
        let mut buf = sink.lock().unwrap_or_else(|e| e.into_inner());
        if buf.len() >= cap {
            return; // потолок длительности достигнут — дальше не копим
        }
        let room = cap - buf.len();
        buf.extend_from_slice(&data[..data.len().min(room)]);
    }

    pub(super) fn stop(app: &tauri::AppHandle) -> AppResult<String> {
        let rec = RECORDING
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .ok_or_else(|| AppError::unknown("Запись не начиналась."))?;
        // Просим поток-владельца закрыть микрофон и отдать записанное. Он же
        // уничтожает поток устройства — в том потоке, где его создал.
        let _ = rec.stop_tx.send(());
        let captured = rec
            .done_rx
            .recv()
            .map_err(|_| AppError::unknown("Запись прервалась — попробуйте ещё раз."))?;

        if captured.samples.is_empty() {
            return Err(AppError::unknown("Ничего не записалось — микрофон молчал."));
        }
        let mono = to_mono(&captured.samples, captured.channels);
        let audio = resample_to_16k(&mono, captured.input_hz)?;
        // Очень короткое нажатие — это промах по кнопке, а не речь. Распознавание
        // такого куска даёт мусор, поэтому честно молчим.
        if audio.len() < TARGET_HZ as usize / 2 {
            return Err(AppError::unknown("Слишком короткая запись — скажите фразу подольше."));
        }
        // Тишину до распознавания НЕ пускаем — см. is_silence.
        let (peak, rms) = super::level(&audio);
        crate::journal::info(
            "голос",
            format!("запись {:.1} с, пик {peak:.4}, СКЗ {rms:.5}", audio.len() as f32 / TARGET_HZ as f32),
        );
        if super::is_silence(peak, rms) {
            return Err(AppError::unknown(
                "Микрофон не слышно — в записи тишина. Проверьте, что нужный микрофон \
                 выбран в настройках системы, не выключен и что приложению разрешён \
                 доступ к нему.",
            ));
        }
        transcribe(app, &audio)
    }


    /// Сводим каналы в моно усреднением: распознаванию нужен один канал, а брать
    /// только первый — значит потерять половину сигнала на некоторых микрофонах.
    fn to_mono(data: &[f32], channels: u16) -> Vec<f32> {
        if channels <= 1 {
            return data.to_vec();
        }
        let n = channels as usize;
        data.chunks(n).map(|c| c.iter().sum::<f32>() / c.len() as f32).collect()
    }

    fn resample_to_16k(input: &[f32], from_hz: u32) -> AppResult<Vec<f32>> {
        if from_hz == TARGET_HZ {
            return Ok(input.to_vec());
        }
        use rubato::Resampler;
        let ratio = TARGET_HZ as f64 / from_hz as f64;
        let mut r = rubato::FastFixedIn::<f32>::new(
            ratio,
            1.0,
            rubato::PolynomialDegree::Cubic,
            input.len().max(1),
            1,
        )
        .map_err(|e| AppError::unknown(format!("Не удалось подготовить звук: {e}")))?;
        let out = r
            .process(&[input.to_vec()], None)
            .map_err(|e| AppError::unknown(format!("Не удалось пересчитать частоту звука: {e}")))?;
        Ok(out.into_iter().next().unwrap_or_default())
    }

    fn transcribe(app: &tauri::AppHandle, audio: &[f32]) -> AppResult<String> {
        let path = super::model_path(app)?;
        if !path.is_file() {
            return Err(AppError::new(
                ErrorCode::ModelMissing,
                "Модель распознавания речи не установлена. Откройте Настройки → «Модели» \
                 и установите её — либо с флешки, либо из интернета.",
            ));
        }
        let mut slot = MODEL.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_none() {
            let params = whisper_rs::WhisperContextParameters::default(); // без GPU
            let ctx = whisper_rs::WhisperContext::new_with_params(
                &path,
                params,
            )
            .map_err(|e| {
                AppError::unknown(format!("Не удалось загрузить модель распознавания: {e}"))
            })?;
            *slot = Some(LoadedModel { ctx, last_used: Instant::now() });
        }
        let loaded = slot.as_mut().expect("модель только что загружена");
        loaded.last_used = Instant::now();

        let mut state = loaded
            .ctx
            .create_state()
            .map_err(|e| AppError::unknown(format!("Сбой распознавания: {e}")))?;
        let mut params =
            whisper_rs::FullParams::new(whisper_rs::SamplingStrategy::Greedy { best_of: 1 });
        params.set_language(Some("ru"));
        params.set_translate(false);
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        // ── Против выдуманного текста ──────────────────────────────────────
        // Whisper на плохом или пустом сигнале не молчит, а сочиняет — обычно
        // фразы из субтитровых корпусов, на которых учился. Тишину до сюда не
        // пускает is_silence, но диктовка бывает и на грани: полушёпот, дальний
        // микрофон, вентилятор. Три рычага, снижающие сочинительство:
        params.set_suppress_blank(true); // не начинать сегмент с пустоты
        params.set_suppress_nst(true); // подавлять неречевые токены
        params.set_no_speech_thold(0.6); // выше порога — считать, что речи нет
        // Половина ядер: распознавание не должно отбирать процессор у генерации,
        // если пользователь начал диктовать, не дождавшись предыдущего ответа.
        let threads = (std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4) / 2).max(1);
        params.set_n_threads(threads as i32);

        state
            .full(params, audio)
            .map_err(|e| AppError::unknown(format!("Не удалось распознать речь: {e}")))?;

        let mut text = String::new();
        for i in 0..state.full_n_segments() {
            if let Some(seg) = state.get_segment(i) {
                // to_str_lossy: экзотическая кодировка в сегменте не должна ронять
                // всю диктовку — лучше показать текст с заменой битого символа.
                if let Ok(s) = seg.to_str_lossy() {
                    text.push_str(&s);
                }
            }
        }
        Ok(text.trim().to_string())
    }

    /// Закрыть микрофон и ВЫБРОСИТЬ записанное. Отдельно от stop(), потому что
    /// «передумал» и «сказал» — разные намерения: распознавать выброшенное значит
    /// вписать в поле ввода то, чего человек говорить не собирался.
    ///
    /// Молчит, если записи не было: отмену интерфейс шлёт и вслепую (уход из окна,
    /// Esc), и отказ на это был бы шумом на ровном месте.
    pub(super) fn cancel() {
        let Some(rec) = RECORDING.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            return;
        };
        let _ = rec.stop_tx.send(());
        // Дожидаемся ответа потока-владельца: он закрывает микрофон и отдаёт буфер.
        // Ждать обязательно — иначе следующая запись начнётся, пока устройство ещё
        // открыто предыдущей, и получит отказ на пустом месте.
        let _ = rec.done_rx.recv();
    }

    /// Выгрузить распознаватель, если им давно не пользовались. Зовётся из фонового
    /// присмотра: держать сотни мегабайт «на всякий случай» — ровно та беспечность,
    /// из-за которой у пользователей и переполнялась память.
    pub(super) fn release_idle() {
        let mut slot = MODEL.lock().unwrap_or_else(|e| e.into_inner());
        let stale = slot.as_ref().is_some_and(|m| m.last_used.elapsed() > MODEL_IDLE_TIMEOUT);
        if stale {
            *slot = None;
        }
    }
}

/// Начать запись с микрофона.
#[tauri::command]
pub(crate) async fn voice_start(app: tauri::AppHandle) -> AppResult<()> {
    tauri::async_runtime::spawn_blocking(move || imp::start(&app))
        .await
        .map_err(|e| AppError::unknown(format!("Сбой запуска записи: {e}")))?
}

/// Остановить запись и вернуть распознанный текст.
/// Тяжёлая операция (секунды на процессоре) — уводим из асинхронного воркера.
#[tauri::command]
pub(crate) async fn voice_stop(app: tauri::AppHandle) -> AppResult<String> {
    tauri::async_runtime::spawn_blocking(move || imp::stop(&app))
        .await
        .map_err(|e| AppError::unknown(format!("Сбой распознавания: {e}")))?
}

/// Отменить запись: закрыть микрофон и выбросить записанное, ничего не распознавая.
/// Ошибок не возвращает — отмена без записи это не ошибка, а обычное дело.
#[tauri::command]
pub(crate) async fn voice_cancel() {
    let _ = tauri::async_runtime::spawn_blocking(imp::cancel).await;
}

/// Фоновый присмотр за памятью распознавателя. Запускается один раз при старте
/// приложения: раз в минуту проверяет, не простаивает ли модель дольше отведённого,
/// и освобождает сотни мегабайт. Без этого распознаватель, поднятый одной диктовкой,
/// висел бы в памяти до закрытия приложения — ровно та беспечность, из-за которой у
/// пользователей и переполнялась память.
pub(crate) fn spawn_idle_watch() {
    tauri::async_runtime::spawn(async {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            imp::release_idle();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("jai-voice-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Скачанный файл с несошедшейся суммой (обрыв, подмена, сбой диска) не должен
    /// оказаться под именем модели: иначе распознавание падало бы у клиента, а
    /// причина выглядела бы как поломка голосового ввода.
    #[test]
    fn corrupted_download_is_rejected_and_removed() {
        let dir = tmp("bad-download");
        let target = dir.join(MODEL_FILE);
        let part = part_path(&target);
        std::fs::write(&part, "половина файла".as_bytes()).unwrap();

        let err = publish_verified(&part, &target, &"0".repeat(64)).unwrap_err();
        assert!(
            err.message.contains("контрольная сумма не сходится"),
            "ошибка объясняет причину: {}",
            err.message
        );
        assert!(!target.exists(), "битая загрузка не опубликована под именем модели");
        assert!(!part.exists(), "частичный файл убран");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Обратная сторона той же проверки: при совпадении суммы файл встаёт на место.
    #[test]
    fn verified_download_is_published() {
        let dir = tmp("good-download");
        let target = dir.join(MODEL_FILE);
        let part = part_path(&target);
        std::fs::write(&part, "модель".as_bytes()).unwrap();

        publish_verified(&part, &target, MODEL_SHA256).unwrap();
        assert!(target.is_file(), "проверенный файл переименован в модель");
        assert!(!part.exists(), "временного файла не остаётся");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Потолок длительности задан ДВАЖДЫ: здесь (на нём обрывается накопление
    /// буфера) и в src/voice.ts (на нём закрывается запись и идёт обратный отсчёт).
    /// Разъедутся — либо счётчик врёт пользователю, либо запись «идёт», а звук уже
    /// не пишется: человек договаривает в тишину и теряет сказанное.
    #[cfg(feature = "voice")]
    #[test]
    fn recording_cap_matches_frontend() {
        let ts = include_str!("../../src/voice.ts");
        let line = ts
            .lines()
            .find(|l| l.trim_start().starts_with("const MAX_RECORDING_SECS"))
            .expect("в src/voice.ts нет MAX_RECORDING_SECS");
        let value: usize = line
            .split('=')
            .nth(1)
            .and_then(|v| v.trim().trim_end_matches(';').parse().ok())
            .expect("не разобрать значение MAX_RECORDING_SECS");
        assert_eq!(
            value, MAX_RECORDING_SECS,
            "потолок записи разошёлся: Rust {MAX_RECORDING_SECS} с, фронтенд {value} с"
        );
    }

    /// Тишина не должна доходить до распознавания.
    ///
    /// Отказ, который это ловит, случился живьём: на macOS в бандле не было ключа
    /// NSMicrophoneUsageDescription, система молча отдавала нули вместо звука, а
    /// whisper на тишине выдумывал текст — на ЛЮБУЮ диктовку пользователь получал
    /// «Редактор субтитров А.Семкин Корректор А.Егорова». Выглядит это не как
    /// «микрофон недоступен», а как «программа несёт чушь».
    #[cfg(feature = "voice")]
    #[test]
    fn silence_is_rejected_before_recognition() {
        use super::{is_silence, level};

        // Ровно то, что отдаёт система при запрещённом доступе к микрофону.
        let digital_silence = vec![0.0f32; 16_000];
        let (p, r) = level(&digital_silence);
        assert!(is_silence(p, r), "цифровая тишина обязана отсекаться");

        // Очень тихий фон (шум звуковой карты) — тоже не речь.
        let hiss: Vec<f32> =
            (0..16_000).map(|i| if i % 2 == 0 { 0.0005 } else { -0.0005 }).collect();
        let (p, r) = level(&hiss);
        assert!(is_silence(p, r), "фоновый шум не должен уходить в распознавание");

        // А вот это уже речь по уровню — отсекать нельзя, иначе диктовка сломается
        // у тех, кто говорит тихо или сидит далеко от микрофона.
        let speech: Vec<f32> = (0..16_000)
            .map(|i| (i as f32 * 0.05).sin() * 0.08) // ≈ −22 dBFS, негромкий голос
            .collect();
        let (p, r) = level(&speech);
        assert!(!is_silence(p, r), "тихую речь глушить нельзя: пик {p}, СКЗ {r}");
    }

    /// Те же ссылка, размер и сумма заданы ВТОРОЙ раз — в сборщике флешки
    /// (tools/make-usb.sh), который кладёт модель на носитель. Разъедутся — флешка
    /// соберётся с файлом, который приложение отвергнет по контрольной сумме уже у
    /// клиента, где перекачать нечем. Держать два списка нельзя, а поскольку один из
    /// них shell, а другой Rust, — сверяем их тестом.
    #[test]
    fn usb_builder_and_app_agree_on_the_model() {
        let sh = include_str!("../../tools/make-usb.sh");
        let value = |key: &str| -> String {
            sh.lines()
                .find_map(|l| l.trim().strip_prefix(&format!("{key}=")))
                .unwrap_or_else(|| panic!("в make-usb.sh нет {key}"))
                .trim()
                .trim_matches('"')
                .to_string()
        };
        assert_eq!(value("VOICE_URL"), MODEL_URL, "ссылка на модель разошлась с флешкой");
        assert_eq!(
            value("VOICE_SHA"),
            MODEL_SHA256,
            "контрольная сумма модели разошлась с флешкой"
        );
        assert_eq!(
            value("VOICE_SIZE").parse::<u64>().expect("VOICE_SIZE — число"),
            MODEL_BYTES,
            "размер модели разошёлся с флешкой"
        );
        assert!(
            sh.contains(MODEL_FILE),
            "make-usb.sh кладёт файл под другим именем, чем ищет приложение"
        );
    }

    /// Ссылка и контрольная сумма описывают ОДИН файл, и правят их вручную:
    /// опечатка в них означает либо «сумма никогда не сойдётся», либо, что хуже,
    /// проверку, которая ничего не проверяет.
    #[test]
    fn model_source_and_checksum_agree() {
        assert_eq!(MODEL_SHA256.len(), 64, "sha256 — 64 шестнадцатеричных знака");
        assert!(MODEL_SHA256.bytes().all(|b| b.is_ascii_hexdigit()), "sha256 — только hex");
        assert!(MODEL_URL.starts_with("https://"), "наружу — только по https");
        assert!(MODEL_URL.ends_with(MODEL_FILE), "ссылка ведёт на файл модели");
    }
}
