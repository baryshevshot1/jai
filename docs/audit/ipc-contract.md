# IPC-контракт jai — машинная сверка (этап A1)

Ревизия `3ae5bbe`, 2026-08-04. Метод: 6 агентов извлечения (Rust-сигнатура ↔ все вызовы invoke) + адверсарная верификация каждого заявленного расхождения повторным чтением обеих сторон. Правила соответствия — Tauri 2 (camelCase JS ↔ snake_case Rust у аргументов при отсутствии rename_all; serde-атрибуты структур ответа; инжектируемые State/AppHandle/Window не передаются; Channel передаётся).

**Счёт: команд 54; вызовов invoke на фронте 85; несвязанных команд 0; неизвестных имён 0; висячих промисов 0; подтверждённых расхождений контракта 5; дефектов, найденных верификаторами сверх заявленного, 9 (см. списки).**

## Полная таблица

| Команда | Rust | Аргументы (Rust → ключ JS) | Возврат (Ok / Err) | Вызовы с фронта | Обработка ошибки | Статус |
|---|---|---|---|---|---|---|
| `agentic_chat` | src-tauri/src/agent.rs:71 | `model:String`→`model`; `messages:Vec<serde_json::Value>`→`messages`; `think:bool`→`think`; `num_ctx:Option<u64>`→`numCtx`; `gentle:Option<bool>`→`gentle`; `on_event:Channel<ChatEvent>`→`onEvent` | AppResult<String>; online-mode-off refusal is Err(&str.into()) → AppError{code:'unknown', message} (error.rs:79-83) | src/chat.ts:718 | same try/catch as chat_stream | чисто |
| `assess_models` | ? | — | Result<Vec<ModelAssessment>, String>; ModelAssessment (provision.rs:744-755, no serde attrs): tag/role/title: String, required/installed: bool, size_gb: f64, approx: bool, verdict: String ("ok"\|"tight"\|"no"), limiting… | ? | try | чисто |
| `build_info` | ? | — | ? | ? | enclosing try/catch | чисто |
| `cancel_pull` | src-tauri/src/models.rs:205 | — | () — infallible, no Result | src/pull.ts:104; src/settings.ts:853 | catch-swallowed intentionally | чисто |
| `cancel_stream` | src-tauri/src/chat.rs:408 | — | () — infallible, no Result | src/ui.ts:477 | catch-swallowed intentionally | чисто |
| `chat_stream` | src-tauri/src/chat.rs:139 | `model:String`→`model`; `messages:Vec<ChatMessage>`→`messages`; `think:bool`→`think`; `num_ctx:Option<u64>`→`numCtx`; `gentle:Option<bool>`→`gentle`; `on_event:Channel<ChatEvent>`→`onEvent` | AppResult<String> — T=String (full answer text); E=AppError {code: snake_case string, message: String} (error.rs:34-38, codes error.rs:17-30) | src/chat.ts:718 | enclosing try/catch | чисто |
| `check_model_updates` | ? | — | AppResult<Vec<UpdateStatus>>; UpdateStatus (models.rs:380-386, no rename_all): tag: String, status: String ("not_installed"\|"unsupported"\|"current"\|"update"\|"error"), message: Option<String> with #[serde(skip_serializ… | ? | try | ⚠ type-drift |
| `clear_conversations` | src-tauri/src/history.rs | — | ? | src/conversations.ts | try/catch | чисто |
| `clear_engine_overrides` | src-tauri/src/settings.rs:109 | — | ? | ? | try/catch | чисто |
| `clear_outbound_log` | src-tauri/src/agent.rs:363 | — | Result<(), String> — E is plain String | src/online.ts:85 | try/catch | чисто |
| `delete_conversation` | src-tauri/src/history.rs | `id:String`→`id` | ? | src/conversations.ts | try/catch | чисто |
| `delete_document` | ? | `id:i64`→`id` | ? | ? | try/catch at documents.ts:260-266; flashIndexLabel | чисто |
| `delete_project` | src-tauri/src/projects.rs | `id:String`→`id` | ? | src/projects.ts | try/catch | чисто |
| `detect_hardware` | ? | — | Result<HardwareInfo, String>; HardwareInfo (diagnostics.rs:225-232, no serde attrs): ram_gb: f64, cpu_cores: usize, vram_gb: Option<f64> (null when None), vram_free_gb: Option<f64> (null), vram_source: String, tier: S… | ?; ? | try | чисто |
| `document_fragments` | ? | `filename:String`→`filename`; `chunk_indexes:Vec<i64>`→`chunkIndexes`; `project_id:Option<String>`→`projectId` | ? | ? | try/catch in loadFragments at ui.ts:607-630; panel.textConte | чисто |
| `documents_empty` | ? | `project_id:Option<String>`→`projectId` | ? | ? | try/catch at documents.ts:274-283; error goes to console.err | чисто |
| `embedding_status` | ? | — | bool (bare, no Result — command itself never errors; network failures inside embed::is_available become false) | ?; ? | inner try; try | чисто |
| `ensure_engine` | src-tauri/src/settings.rs:121 | — | AppResult<engine::EngineStatus>; EngineStatus (engine.rs:64-67): status:String, message:String, plain Serialize no rename — matches inline TS type at models.ts:616; body always returns Ok, so E=AppError is theoretical | src/models.ts:618 | try/catch | чисто |
| `extract_document` | src-tauri/src/documents.rs:40 | `path:String`→`path` | Result<DocumentText, String>; DocumentText (documents.rs:22-28): name:String, ext:String, text:String, chars:usize, plain Serialize no rename — matches inline TS type at attachments.ts:120; E is plain String | src/attachments.ts:122 | try/catch | чисто |
| `find_model_sources` | ? | — | Vec<ModelSource> (infallible — no Result); ModelSource (provision.rs:36-42, no serde attrs): path: String, removable: bool, models: u64, total_gb: f64 | ?; ? | try | чисто |
| `find_voice_model_source` | ? | — | ? | ? | enclosing try/catch | чисто |
| `get_outbound_log` | src-tauri/src/agent.rs:355 | — | Vec<OutboundLogEntry> (infallible); OutboundLogEntry (agent.rs:299-304): ts:i64, host:String, tool:String, query:String, plain Serialize no rename — matches TS OutboundLogEntry (src/types.ts:173-178) | src/online.ts:98 | try/catch | чисто |
| `get_setting` | src-tauri/src/settings.rs | `key:String`→`key` | ? | src/models.ts; src/online.ts; src/online.ts; src/settings.ts; src/settings.ts; src/settings.ts; src/settings.ts; src/settings.ts; src/settings.ts; src/settings.ts; src/settings.ts | try/catch; общий try/catch; тот же catch | чисто |
| `import_models_from_dir` | ? | `:`→`path`; `:`→`onEvent (camelCase → snake_case default mapping)` | Result<PullOutcome, String>; PullOutcome enum (models.rs:57-62) #[serde(rename_all = "lowercase")] → "done" \| "cancelled". Channel payload PullEvent (models.rs:44-52) #[serde(tag="type", rename_all="lowercase")] → { t… | ? | try | чисто |
| `import_voice_model` | ? | — | ? | ? | same runVoiceInstall try/catch: humanError | чисто |
| `index_document` | ? | `path:String`→`path`; `project_id:Option<String>`→`projectId`; `on_progress:Channel<IndexProgress>`→`onProgress (Channel from @tauri-apps/api/core; events {phase: string, current: usize, total: usize})` | ? | ? | try/catch at documents.ts:225-255; flashIndexLabel | чисто |
| `install_update_from_disk` | ? | — | ? | ? | enclosing try/catch | чисто |
| `journal_log` | ? | — | ? | ? | .catch | чисто |
| `journal_path` | ? | — | ? | ?; ? | enclosing try/catch | чисто |
| `journal_tail` | ? | — | ? | ? | enclosing try/catch | чисто |
| `list_conversations` | src-tauri/src/history.rs | — | ? | src/conversations.ts; src/conversations.ts | try/catch | ⚠ no-catch; ⚠ no-catch |
| `list_documents` | ? | `project_id:Option<String>`→`projectId` | ? | ? | try/catch at documents.ts:89-97; flashIndexLabel | чисто |
| `list_models` | ? | — | ? | ? | try/catch at models.ts:266-274; addError | чисто |
| `list_projects` | src-tauri/src/projects.rs | — | ? | src/projects.ts | try/catch | ⚠ no-catch |
| `load_conversation` | src-tauri/src/history.rs | `id:String`→`id` | ? | src/conversations.ts | try/catch | чисто |
| `model_states` | ? | — | ? | ?; ?; ?; ? | try/catch at models.ts:338-347; modelsStatus; try/catch at models.ts:95-100; swallowed but titlesAsked res; try/catch at wizard.ts:346-369; catch pushes an honest info ; try/catch at wizard.ts:50-57; error swallowed with explanato | чисто |
| `ollama_version` | ? | — | Result<String, String> — plain version string | ?; ? | try | чисто |
| `plan_inference` | ? | `:`→`model` | AppResult<InferencePlan>; InferencePlan (memory.rs:420-428, no serde attrs): action: String, model: String, num_ctx: u64, reason: Option<String> (serialized as null when None — no skip attr), original_model: String, n… | ? | try | чисто |
| `pull_model` | src-tauri/src/models.rs:70 | `name:String`→`name`; `on_event:Channel<PullEvent>`→`onEvent` | AppResult<PullOutcome>; PullOutcome (models.rs:57-62) rename_all=lowercase → 'done'\|'cancelled', matches TS PullOutcome (src/types.ts:54); E=AppError | src/pull.ts:84 | try/catch | чисто |
| `read_image_base64` | ? | `path:String`→`path` | ? | ? | try/catch at attachments.ts:175-180; addError | чисто |
| `reload_engine` | src-tauri/src/settings.rs:156 | — | ? | ?; ? | try/catch | ⚠ type-drift |
| `run_diagnostics` | ? | — | AppResult<Vec<DiagCheck>>; DiagCheck (diagnostics.rs:519-524, no serde attrs): id/title/status: &'static str, detail: String. Body never constructs Err — every failure becomes a "fail" row; unconditional Ok(out) at di… | ? | try | ⚠ type-drift |
| `save_conversation` | src-tauri/src/history.rs | `conversation:Conversation`→`conversation` | ? | src/conversations.ts | try/catch | чисто |
| `save_project` | src-tauri/src/projects.rs | `project:Project`→`project` | ? | src/projects.ts; src/projects.ts | try/catch | чисто |
| `search_documents` | ? | `query:String`→`query`; `k:usize`→`k`; `project_id:Option<String>`→`projectId` | ? | ? | try/catch at chat.ts:417-436; notify | чисто |
| `set_engine_path` | src-tauri/src/settings.rs:87 | `path:String`→`path` | ? | ? | try/catch | чисто |
| `set_models_dir` | src-tauri/src/settings.rs:98 | `path:String`→`path` | ? | ? | try/catch | чисто |
| `set_setting` | src-tauri/src/settings.rs:63 | `key:String`→`key`; `value:String`→`value` | ? | ?; ?; ?; ?; ?; ?; ?; ?; ?; ?; ? | .catch | чисто |
| `voice_available` | src-tauri/src/voice.rs:75 | — | ? | ? | try/catch | чисто |
| `voice_cancel` | src-tauri/src/voice.rs:712 | — | ? | ? | try/catch | чисто |
| `voice_model_download` | ? | — | ? | ? | try/catch in runVoiceInstall | чисто |
| `voice_model_state` | ? | — | ? | ? | enclosing try/catch | чисто |
| `voice_start` | src-tauri/src/voice.rs:694 | — | ? | ? | .then | чисто |
| `voice_stop` | src-tauri/src/voice.rs:703 | — | ? | ? | try/catch/finally | чисто |

## Расхождения по классам (только подтверждённые верификацией)

### 1. Расхождения имён/регистра аргументов (camelCase/snake_case)
**Не найдено.** Все 85 вызовов передают ключи, совпадающие с ожиданиями Tauri 2; `rename_all` не используется нигде, многословные параметры везде переданы camelCase (`numCtx`, `onEvent`, `projectId` и т.д.).

### 2. Расхождения типов результата (type-drift)
- `check_model_updates` — TS-generic на src/models.ts:490 не содержит необязательного поля `message`, которое Rust сериализует при статусах "error"/"unsupported" (src-tauri/src/models.rs:384-385). Ошибки не будет, но причина отказа проверки обновлений невидима для UI. → F-FRONTEND-008
- `run_diagnostics` — канал ошибки: AppError сериализуется объектом `{code, message}` (error.rs:35-39), а src/settings.ts:358 интерполирует значение сырым `${e}` → «[object Object]». Смягчение: тело команды всегда возвращает Ok, дефект проявится только на IPC-отказе. → F-FRONTEND-005
- Примечание: заявление «то же на settings.ts:290» верификация ОПРОВЕРГЛА как type-drift для основной ветки (`String(e)` корректен для соседней `clear_engine_overrides` с E=String); реальный дефект этих строк — потеря in-band-статуса reload_engine, см. класс 5.

### 3. Необязательные поля, где стороны расходятся
- Единственный случай — `message?` у `check_model_updates` (см. выше). Остальные Option/skip_serializing_if сверены — расхождений нет.

### 4. Команды без обработки ошибки на фронте / вызовы без await
- Формально «без catch» и «без await» — **ноль из 85**. Fire-and-forget с намеренным `.catch(() => {})` — 3 места, все обоснованы комментариями: ui.ts:477 (`cancel_stream`), pull.ts:104 и settings.ts:853 (`cancel_pull`) — команды unit-типа, итог приходит результатом основной команды.
- Однако «catch есть, но глотает» — 3 подтверждённых места, где ошибка чтения превращается в «пусто» без следа: conversations.ts:110-112, conversations.ts:361-363, projects.ts:35-38. → F-FRONTEND-002 (P1)

### 5. Найдено верификаторами сверх заявленного (глубокая перепроверка «чистых» команд)
- `clear_conversations` — **ложный успех**: history.rs:146-158 глотает отказ read_dir и пофайловые отказы remove_file (`let _ =`); Ok(()) всегда. Контраст: delete_conversation пробрасывает Err (history.rs:140), delete_project отказывается при неудачной чистке со ссылкой на «удалил — значит удалено» (projects.rs:125-141). → F-BACKEND-001 (P1)
- `reload_engine` — статус движка приходит in-band (Ok(EngineStatus{status:"error"|"not_installed", message})), и оба вызова его теряют: resetEnginePaths показывает успех-тост при упавшем движке (settings.ts:285-288); applyModelsDir при status≠"external" ставит ложный диагноз «bge-m3 не найдена» (settings.ts:155-157). → F-FRONTEND-003
- `save_project` — «Инструкции сохранены» показывается ДО исхода invoke (projects.ts:175-184, без await); таймер projectStatus (projects.ts:95-100) может погасить последующую ошибку; несохранённое состояние не откатывается и блокирует повторную запись сравнением (projects.ts:165,179). → F-FRONTEND-004
- `import_models_from_dir` — финальная сводка импорта («Импортировано моделей: N…», provision.rs:192-200) недостижима для пользователя: settled-guard фронта (pull.ts:55-66) отбрасывает события после результата команды. → F-FRONTEND-006
- `voice_start` — обработчик позднего reject защищён только глобальной `phase` без токена попытки (voice.ts:220-233): поздний отказ попытки №1 может сбросить UI попытки №2 при живой записи — микрофон открыт, интерфейс показывает «выключено», закрыть нечем. Окно узкое, нужен рантайм. → F-FRONTEND-007 (V-008)
- `install_update_from_disk` — нет гейта повторного входа (двойной клик → два диалога; update.rs:17-63, settings.ts:633-657) и блокирующий `xdg-open` `.status()` в async-команде (update.rs:104-108). → F-BACKEND-002
- `set_setting` — блокирующий файловый I/O в синхронной команде (все 11 вызовов — информационно; вынесено в A2).

### 6. Канал событий
Классических emit/listen нет вовсе — весь стрим на `tauri::ipc::Channel` (ChatEvent, PullEvent, IndexProgress; формы payload сверены — расхождений нет; утечек подписок нет: Channel v2 сам чистит колбэк, «хвост» после финала везде гасится settled/opGen-гвардами). Мёртвое на приёме: `ChatEvent::Done` (излучается chat.rs:313, обработчика нет — осознанно, финал по результату команды); фантом документации: фаза "store" в комментарии documents.rs:275 никогда не шлётся.
