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
// Mutex нужен только реализации с распознаванием: без фичи voice модуль сводится
// к честным отказам, и лишнее не должно висеть мёртвым кодом. Instant же нужен и
// скачиванию модели, которое работает независимо от фичи.
#[cfg(feature = "voice")]
use std::sync::Mutex;
use std::time::{Duration, Instant};

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
const MODEL_URL: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.bin";

/// sha256 файла ggml-small.bin (он же oid LFS в Hugging Face). Одно значение на оба
/// пути установки — и для скачанного файла, и для копии с носителя.
pub(crate) const MODEL_SHA256: &str =
    "1be3a9b2063867b937e64e2ec7483364a79917e157fa98c5d94b5c1fffea987b";

/// Размер файла, байт. Нужен, когда сервер не сообщает длину ответа: без него
/// прогресс не показать, а пользователь ждёт почти полгигабайта. Он же отсекает
/// подсунутый по ошибке чужой файл ещё до копирования с флешки.
pub(crate) const MODEL_BYTES: u64 = 487_601_967;

/// Прогресс шлём не чаще, чем раз в столько байт: каждое событие — сообщение в
/// окно, мельчить незачем (то же правило, что при импорте с носителя).
const PROGRESS_STEP: u64 = 8 * 1024 * 1024;

/// Столько подряд без единого байта — честная ошибка вместо вечного «прогресса».
const DOWNLOAD_STALL: Duration = Duration::from_secs(120);

/// Временное имя загрузки: оборванная закачка не должна выглядеть готовой моделью.
fn part_path(target: &Path) -> PathBuf {
    let mut p = target.as_os_str().to_os_string();
    p.push(".part");
    PathBuf::from(p)
}

/// Опубликовать файл под финальным именем — ТОЛЬКО если контрольная сумма сошлась.
/// Не сошлась: файл удаляем и говорим человеку, что делать. Модель, не прошедшая
/// сверку, до распознавания не доходит вовсе.
fn publish_verified(part: &Path, target: &Path, got: &str) -> AppResult<()> {
    if got != MODEL_SHA256 {
        let _ = std::fs::remove_file(part);
        return Err(AppError::unknown(
            "Файл модели распознавания речи получен с ошибкой (контрольная сумма не \
             сходится). Модель не установлена — повторите установку.",
        ));
    }
    let _ = std::fs::remove_file(target); // Windows: rename поверх существующего не работает
    std::fs::rename(part, target)
        .map_err(|e| AppError::unknown(format!("Не удалось сохранить модель: {e}")))?;
    Ok(())
}

/// Скачать модель распознавания речи (около 470 МБ) с прогрессом и отменой.
/// Регистрируется в общем PullState: установка моделей движка и модели
/// распознавания взаимоисключаемы и отменяются одной командой cancel_pull.
/// Наружу идём ИЗОЛИРОВАННЫМ клиентом онлайн-слоя (tools::web_client, https), а не
/// localhost-клиентом Ollama — сетевые пути в проекте не смешиваются.
#[tauri::command]
pub(crate) async fn voice_model_download(
    app: tauri::AppHandle,
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
    let _ = std::fs::remove_file(&part); // остаток прежней оборванной попытки

    let client = crate::tools::web_client()?;
    // Подключение — с отменой и потолком ожидания: сервер может принять соединение
    // и замолчать, а занятая регистрация блокировала бы любые установки.
    let send = tokio::time::timeout(Duration::from_secs(60), client.get(MODEL_URL).send());
    let mut resp = match crate::with_cancel(send, &my_cancel).await {
        None => return Ok(PullOutcome::Cancelled), // «Отмена» ещё до ответа сервера
        Some(Err(_elapsed)) => {
            return Err(AppError::timeout(
                "Сервер моделей не ответил — проверьте интернет и повторите \
                 (либо поставьте модель с флешки).",
            ))
        }
        Some(Ok(r)) => r.map_err(|e| {
            AppError::unknown(format!(
                "Не удалось скачать модель распознавания речи (нужен интернет): {e}"
            ))
        })?,
    };
    if !resp.status().is_success() {
        return Err(AppError::unknown(format!(
            "Сервер моделей вернул ошибку {} — попробуйте позже либо поставьте модель \
             с флешки.",
            resp.status()
        )));
    }

    let total = resp.content_length().unwrap_or(MODEL_BYTES);
    let status = "Скачивание модели распознавания речи";
    let _ = on_event.send(PullEvent::Progress {
        status: status.to_string(),
        completed: 0,
        total,
    });

    use std::io::Write;
    let mut file = std::fs::File::create(&part)
        .map_err(|e| AppError::unknown(format!("Не удалось записать «{}»: {e}", part.display())))?;
    let mut hasher = Sha256::new();
    let mut done: u64 = 0;
    let mut last_sent: u64 = 0;
    let mut last_data = Instant::now();
    // Чанки ждём короткими окнами: отмена срабатывает, ДАЖЕ когда сеть молчит;
    // затянувшееся молчание — честная ошибка вместо вечной «загрузки».
    loop {
        if my_cancel.load(Ordering::Relaxed) {
            drop(file);
            let _ = std::fs::remove_file(&part);
            return Ok(PullOutcome::Cancelled);
        }
        match tokio::time::timeout(Duration::from_millis(400), resp.chunk()).await {
            Err(_elapsed) => {
                if last_data.elapsed() > DOWNLOAD_STALL {
                    drop(file);
                    let _ = std::fs::remove_file(&part);
                    return Err(AppError::timeout(
                        "Сеть не отвечает — загрузка прервана. Проверьте интернет и \
                         повторите.",
                    ));
                }
                continue; // окно без данных — перепроверяем отмену
            }
            Ok(Ok(Some(chunk))) => {
                last_data = Instant::now();
                file.write_all(&chunk).map_err(|e| {
                    AppError::unknown(format!("Сбой записи на диск (хватает ли места?): {e}"))
                })?;
                hasher.update(&chunk);
                done += chunk.len() as u64;
                if done - last_sent >= PROGRESS_STEP {
                    last_sent = done;
                    let _ = on_event.send(PullEvent::Progress {
                        status: status.to_string(),
                        completed: done,
                        // Сервер мог сообщить длину меньше фактической — прогресс
                        // не должен «переползать» за 100 %.
                        total: total.max(done),
                    });
                }
            }
            Ok(Ok(None)) => break, // поток корректно завершился
            Ok(Err(e)) => {
                drop(file);
                let _ = std::fs::remove_file(&part);
                return Err(AppError::unknown(format!("Загрузка прервалась: {e}")));
            }
        }
    }

    // fsync до переименования: после сбоя питания под финальным именем не должен
    // оказаться недописанный файл — он молча сошёл бы за установленную модель.
    file.sync_all()
        .map_err(|e| AppError::unknown(format!("Не удалось сохранить модель на диск: {e}")))?;
    drop(file);
    publish_verified(&part, &target, &crate::provision::hex_digest(hasher))?;
    let _ = on_event.send(PullEvent::Progress {
        status: "Готово".to_string(),
        completed: done,
        total: done,
    });
    Ok(PullOutcome::Done)
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
