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

use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::error::{AppError, AppResult, ErrorCode};

/// Частота, которую ждёт whisper. Микрофоны отдают 44.1/48 кГц — пересчитываем.
const TARGET_HZ: u32 = 16_000;

/// Потолок длительности одной диктовки. Защита от «зажал и забыл»: без неё буфер
/// растёт бесконечно (16 кГц × 4 байта ≈ 3.8 МБ в минуту), а распознавание очень
/// длинной записи занимает минуты и выглядит как зависание.
const MAX_RECORDING_SECS: usize = 120;

/// Сколько распознаватель живёт в памяти без дела. Симметрично keep_alive языковой
/// модели: подряд надиктованные фразы не платят за повторную загрузку, а забытый
/// микрофон не держит сотни мегабайт вечно.
const MODEL_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Имя файла модели в каталоге данных приложения. Кладётся мастером установки
/// (с флешки) или скачивается пользователем — путь один и тот же.
const MODEL_FILE: &str = "ggml-small.bin";

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

    /// Идущая запись: поток устройства и накопленные отсчёты. Поток cpal не Send,
    /// поэтому он живёт в своём потоке, а сюда приходят готовые отсчёты.
    struct Recording {
        samples: std::sync::Arc<Mutex<Vec<f32>>>,
        input_hz: u32,
        channels: u16,
        /// Держим поток живым: его Drop останавливает запись.
        _stream: Box<dyn StreamHandle>,
    }

    /// Обёртка, чтобы хранить поток разного типа отсчётов за одним интерфейсом.
    trait StreamHandle: Send {}
    struct StreamBox(#[allow(dead_code)] cpal::Stream);
    // cpal::Stream не Send, но мы никогда не двигаем его между потоками: он создаётся
    // и уничтожается в одном и том же месте под мьютексом. Обещание безопасно.
    unsafe impl Send for StreamBox {}
    impl StreamHandle for StreamBox {}

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
        let host = cpal::default_host();
        let device = host.default_input_device().ok_or_else(|| {
            AppError::unknown(
                "Микрофон не найден. Проверьте, что устройство записи подключено и \
                 разрешено в настройках системы.",
            )
        })?;
        let cfg = device.default_input_config().map_err(|e| {
            AppError::unknown(format!("Не удалось получить настройки микрофона: {e}"))
        })?;
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
                return Err(AppError::unknown(format!(
                    "Микрофон отдаёт неподдерживаемый формат звука ({other:?})."
                )))
            }
        }
        .map_err(|e| AppError::unknown(format!("Не удалось начать запись: {e}")))?;

        stream
            .play()
            .map_err(|e| AppError::unknown(format!("Не удалось запустить микрофон: {e}")))?;

        *slot = Some(Recording {
            samples,
            input_hz,
            channels,
            _stream: Box::new(StreamBox(stream)),
        });
        Ok(())
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
        // Drop потока останавливает устройство до того, как начнём считать.
        let Recording { samples, input_hz, channels, _stream } = rec;
        drop(_stream);

        let raw = std::mem::take(&mut *samples.lock().unwrap_or_else(|e| e.into_inner()));
        if raw.is_empty() {
            return Err(AppError::unknown("Ничего не записалось — микрофон молчал."));
        }
        let mono = to_mono(&raw, channels);
        let audio = resample_to_16k(&mono, input_hz)?;
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
