// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/
//
// lib.rs — только общее ядро и сборка приложения. Домены разнесены по модулям
// (чат, онлайн-слой, модели, память, документы, история, проекты, настройки):
// здесь остаётся то, что нужно всем сразу, и список команд для фронтенда.
mod agent; // Онлайн-слой (агентный режим): цикл tool calling + журнал исходящих
mod chat; // Чат: стриминг ответа Ollama, «щадящий режим», watchdog, «Стоп»
mod chunk; // Чанкинг (Фаза B): резка текста на фрагменты по границам
mod diagnostics; // Железо и «проверка системы»: детект ресурсов, самопроверка
mod docstore; // База документов (Фаза B): векторное хранилище SQLite + sqlite-vec
mod download; // Докачиваемая загрузка большого файла (проверена на локальном сервере)
mod documents; // Документы: извлечение текста, индексация в базу, поиск
mod embed; // Эмбеддинги (Фаза B): bge-m3 через Ollama, батчем
mod engine; // Операционный слой: управление процессом движка Ollama
mod error; // Ошибки с кодом причины: интерфейс не разбирает текст сообщений
mod history; // История диалогов: файлы appDataDir/conversations/<id>.json
mod journal; // Журнал приложения: файл + stdout + хвост для экрана «Диагностика»
mod memory; // Память: оценка «поместится ли» и лестница смягчения (S1/S2)
mod models; // Модели: установка, набор приложения, состояние, обновления
mod projects; // Проекты: группировка чатов + своя база знаний
mod provision; // Поставка моделей: импорт с флешки/диска, оценка посильности, поиск носителей
mod settings; // Настройки приложения и офлайн-пути движка
mod tools; // Онлайн-слой (агентный режим): исходящий клиент + инструменты (веб-поиск)
mod voice; // Голосовой ввод: запись с микрофона и распознавание речи
mod update; // Обновление приложения с диска (офлайн-путь)

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::LazyLock;
use tauri::Manager;

/// Общий HTTP-клиент к локальному Ollama (один пул соединений на процесс — без
/// пересоздания клиента на каждый вызов). connect_timeout защищает от зависания на
/// подключении; общего таймаута НЕТ — стримы (чат, установка моделей) живут долго.
/// Короткие метаданные-запросы задают свой .timeout(OLLAMA_META_TIMEOUT) на вызов.
/// Наружу (интернет) этот клиент не ходит — только tools::web_client().
pub(crate) static HTTP: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(2))
        .build()
        .expect("не удалось создать HTTP-клиент")
});

/// Таймаут коротких метаданных-запросов к Ollama (/api/tags, /api/ps, /api/show,
/// /api/version): зависший движок не должен вешать список моделей и диагностику.
pub(crate) const OLLAMA_META_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Флаг отмены АКТИВНОГО стрима (нажата «Стоп» ИЛИ сработал watchdog). Внутри —
/// `Mutex<Arc<AtomicBool>>`: каждый новый запрос кладёт сюда СВОЙ свежий флаг, а
/// «Стоп» взводит тот, что активен сейчас. Так новый запрос не «размораживает»
/// старый стрим (у каждого свой Arc), и отмена всегда бьёт по текущему запросу.
pub(crate) struct CancelFlag(pub(crate) std::sync::Mutex<Arc<AtomicBool>>);

impl CancelFlag {
    /// Зарегистрировать новый запрос как активный: создаёт свежий флаг, делает его
    /// текущим (старый остаётся взведённым у своего стрима) и возвращает вызывающему.
    pub(crate) fn begin(&self) -> Arc<AtomicBool> {
        let flag = Arc::new(AtomicBool::new(false));
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = flag.clone();
        flag
    }
}

/// Ждать future, периодически проверяя флаг отмены (каждые 200 мс — тот же приём,
/// что 400-мс опрос чанков в стриме). None = отменено; сброшенный future реально
/// обрывает HTTP-запрос reqwest (соединение закрывается). tokio::select! недоступен
/// (фича `macros` не включена) — опрашиваем через timeout + pin.
pub(crate) async fn with_cancel<F: std::future::Future>(
    fut: F,
    cancel: &AtomicBool,
) -> Option<F::Output> {
    tokio::pin!(fut);
    loop {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        match tokio::time::timeout(std::time::Duration::from_millis(200), &mut fut).await {
            Ok(v) => return Some(v),
            Err(_) => continue, // окно вышло без результата — перепроверяем отмену
        }
    }
}

// ── Общие утилиты хранения: время, атомарная запись, проверка идентификатора ──

pub(crate) fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Атомарная и ДОЛГОВЕЧНАЯ запись: пишем во временный файл, доводим его до диска
/// (fsync) и переименовываем. Атомарность защищает от половинчатого файла при
/// падении приложения; fsync — от потери данных при внезапной перезагрузке ОС
/// (перегрев, питание): без него содержимое могло остаться в кеше ОС. На Unix
/// дополнительно синхронизируем каталог, чтобы пережило ребут само переименование.
pub(crate) fn write_atomic(path: &std::path::Path, content: &str) -> Result<(), String> {
    // Имя tmp уникально на процесс и запись: общий tmp у двух параллельных писателей
    // ОДНОГО файла приводил бы к тому, что второй rename публикует чужое содержимое
    // или падает «файл не найден».
    static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tmp = path.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        TMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&tmp, content).map_err(|e| format!("Не удалось записать файл: {e}"))?;
    std::fs::File::open(&tmp)
        .and_then(|f| f.sync_all())
        .map_err(|e| format!("Не удалось довести файл до диска: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("Не удалось сохранить файл: {e}"))?;
    #[cfg(unix)]
    if let Some(dir) = path.parent() {
        // Ошибку не поднимаем: данные уже на диске, fsync каталога — только про
        // долговечность самого rename (некоторые ФС и так его журналируют).
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
    }
    Ok(())
}

/// Защита от обхода путей: id формируем сами (UUID), но всё равно проверяем.
pub(crate) fn validate_id(id: &str) -> Result<(), String> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err("Некорректный идентификатор диалога".to_string());
    }
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Флаг читаем ДО поднятия приложения: аргументы командной строки не зависят ни
    // от окна, ни от плагинов, а сама проверка выполнится в setup, когда уже известны
    // пути к данным (их считает Tauri, и дублировать этот расчёт нельзя).
    let self_check = std::env::args().any(|a| a == "--self-check");
    docstore::register_vec(); // зарегистрировать sqlite-vec до открытия любых соединений
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        // Навигационный заслон: окно приложения нельзя увести на внешний сайт —
        // разрешён только собственный origin (tauri://localhost; на Windows
        // http://tauri.localhost; в dev — сервер Vite). CSP top-level навигацию не
        // ограничивает, поэтому запрет живёт здесь, на уровне Rust: любой внешний
        // URL (регрессия рендера ссылок, window.location из зависимости, редирект)
        // молча отклоняется. Внешние ссылки открываются только через plugin-opener.
        .plugin(
            tauri::plugin::Builder::<_, ()>::new("navigation-guard")
                .on_navigation(|_webview, url| {
                    url.scheme() == "tauri"
                        || url.host_str() == Some("tauri.localhost")
                        || (cfg!(debug_assertions) && url.host_str() == Some("localhost"))
                })
                .build(),
        )
        .manage(CancelFlag(std::sync::Mutex::new(Arc::new(AtomicBool::new(false)))))
        .manage(models::PullState(std::sync::Mutex::new(None)))
        .manage(settings::SettingsLock(std::sync::Mutex::new(())))
        .manage(projects::ProjectsLock(std::sync::Mutex::new(())))
        .manage(engine::EngineState::new())
        .setup(move |app| {
            // Журнал — первым делом: всё, что случится дальше при старте, должно
            // попасть в файл. Иначе диагностика отказа снова сведётся к просьбе
            // «пришлите текст из консоли разработчика», которой у клиента нет.
            journal::init(app.handle());
            journal::info("старт", format!("jai {} — запуск", env!("CARGO_PKG_VERSION")));
            // Режим самопроверки: напечатать, что это за сборка и всё ли на месте,
            // и выйти. Нужен гейту (smoke-тест собранного бандла) и установщику у
            // клиента — вопрос «а тот ли бинарник запущен» иначе решается памятью.
            if self_check {
                std::process::exit(diagnostics::self_check(app.handle()));
            }
            // Присмотр за памятью распознавателя речи: выгружает модель по простою.
            voice::spawn_idle_watch();
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            chat::chat_stream,
            agent::agentic_chat,
            agent::get_outbound_log,
            agent::clear_outbound_log,
            chat::cancel_stream,
            models::pull_model,
            models::cancel_pull,
            settings::ensure_engine,
            documents::extract_document,
            documents::read_image_base64,
            documents::index_document,
            documents::list_documents,
            documents::delete_document,
            documents::documents_empty,
            documents::search_documents,
            documents::document_fragments,
            models::list_models,
            models::model_states,
            models::check_model_updates,
            provision::import_models_from_dir,
            provision::assess_models,
            provision::find_model_sources,
            memory::plan_inference,
            diagnostics::run_diagnostics,
            diagnostics::ollama_version,
            documents::embedding_status,
            diagnostics::detect_hardware,
            history::list_conversations,
            history::load_conversation,
            history::save_conversation,
            history::delete_conversation,
            history::clear_conversations,
            projects::list_projects,
            projects::save_project,
            projects::delete_project,
            settings::get_setting,
            settings::set_setting,
            settings::set_engine_path,
            settings::set_models_dir,
            settings::clear_engine_overrides,
            settings::reload_engine,
            voice::voice_available,
            voice::voice_start,
            voice::voice_stop,
            voice::voice_cancel,
            voice::voice_model_state,
            voice::voice_model_download,
            provision::import_voice_model,
            provision::find_voice_model_source,
            journal::journal_log,
            journal::journal_tail,
            journal::journal_path,
            diagnostics::build_info,
            update::install_update_from_disk
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|handle, event| {
            // На выходе из приложения — остановить движок, ЕСЛИ его подняли мы сами.
            // Внешний/системный экземпляр не трогаем (Принцип 1).
            if let tauri::RunEvent::Exit = event {
                let state = handle.state::<engine::EngineState>();
                engine::stop_if_ours(&state);
            }
        });
}

// ── Контракт команд: один список, видимый и во время работы ───────────────────
//
// Проверка «интерфейс зовёт только то, что зарегистрировано» была, но опиралась на
// разбор ИСХОДНИКА (tools/check-dom.py). Это отвечало на вопрос «что написано», а
// вопрос стоял другой: «что попало в бинарник». Здесь список команд существует как
// данные — его показывает экран «Диагностика» и сверяет `--self-check`, а тест ниже
// не даёт ему разойтись с generate_handler!.
pub(crate) const COMMAND_NAMES: &[&str] = &[
    "chat_stream",
    "agentic_chat",
    "get_outbound_log",
    "clear_outbound_log",
    "cancel_stream",
    "pull_model",
    "cancel_pull",
    "ensure_engine",
    "extract_document",
    "read_image_base64",
    "index_document",
    "list_documents",
    "delete_document",
    "documents_empty",
    "search_documents",
    "document_fragments",
    "list_models",
    "model_states",
    "check_model_updates",
    "import_models_from_dir",
    "assess_models",
    "find_model_sources",
    "plan_inference",
    "run_diagnostics",
    "ollama_version",
    "embedding_status",
    "detect_hardware",
    "list_conversations",
    "load_conversation",
    "save_conversation",
    "delete_conversation",
    "clear_conversations",
    "list_projects",
    "save_project",
    "delete_project",
    "get_setting",
    "set_setting",
    "set_engine_path",
    "set_models_dir",
    "clear_engine_overrides",
    "reload_engine",
    "voice_available",
    "voice_start",
    "voice_stop",
    "voice_cancel",
    "voice_model_state",
    "voice_model_download",
    "import_voice_model",
    "find_voice_model_source",
    "journal_log",
    "journal_tail",
    "journal_path",
    "build_info",
    "install_update_from_disk",
];

#[cfg(test)]
mod command_contract {
    /// Список COMMAND_NAMES обязан совпадать с generate_handler! — иначе «Диагностика»
    /// и `--self-check` рапортовали бы о командах, которых в бинарнике нет (или молчали
    /// бы о тех, что есть). Сверяем с СОБСТВЕННЫМ исходником: второй список, который
    /// правят руками, рано или поздно разъезжается с первым — здесь это уронит тест.
    #[test]
    fn declared_names_match_generate_handler() {
        let src = include_str!("lib.rs");
        let start = src.find("generate_handler![").expect("generate_handler! не найден");
        let rest = &src[start + "generate_handler![".len()..];
        let end = rest.find(']').expect("не найдена закрывающая скобка списка команд");
        let registered: Vec<String> = rest[..end]
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.rsplit("::").next().unwrap().to_string())
            .collect();

        assert_eq!(
            registered,
            super::COMMAND_NAMES,
            "COMMAND_NAMES разошёлся с generate_handler!"
        );
    }
}
