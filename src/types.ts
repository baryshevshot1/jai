// Общие типы приложения (контракты данных между модулями и с Rust-бэкендом).
// Только типы и интерфейсы — код и константы живут в своих модулях.

// Документ, привязанный к сообщению пользователя. text — уже усечённый под бюджет
// фрагмент (его «видит» модель); chars — полный размер исходника (для подписи).
export interface DocAttachment {
  name: string;
  ext: string;
  text: string;
  chars: number;
  truncated: boolean;
}

// Источник ответа (документ + № фрагмента) — для показа под ответом и истории.
export interface SourceRef {
  filename: string;
  chunk_index: number;
}

// Найденный фрагмент из Rust (search_documents).
export interface RetrievedChunk {
  text: string;
  filename: string;
  chunk_index: number;
  page: number | null;
  distance: number;
}

// Карточка документа базы (list_documents).
export interface DocumentMeta {
  id: number;
  filename: string;
  ext: string;
  added_at: number;
  char_count: number;
  chunk_count: number;
}

// Прогресс индексации (Channel из index_document).
export interface IndexProgress {
  phase: string;
  current: number;
  total: number;
}

// Прогресс установки модели (Channel из pull_model). Итог (успех/отмена) приходит
// РЕЗУЛЬТАТОМ команды pull_model — по нему честно отличаем «установлено» от отмены.
export interface PullEvent {
  type: "progress";
  status: string;
  completed: number;
  total: number;
}
export type PullOutcome = "done" | "cancelled";

// Источник из веб-поиска (онлайн-режим): заголовок + ссылка (см. WebSource в tools.rs).
export interface WebSource {
  title: string;
  url: string;
}

// События из Rust (см. ChatEvent в lib.rs). status/sources — только онлайн-слой.
export type ChatEvent =
  | { type: "chunk"; content: string }
  | { type: "thinking"; content: string }
  | { type: "status"; content: string }
  | { type: "sources"; items: WebSource[] }
  | { type: "notice"; content: string }
  | { type: "done" };

export type Role = "user" | "assistant";
export interface Message {
  role: Role;
  content: string;
  doc?: DocAttachment; // только у реплик пользователя, к которым приложен файл
  sources?: SourceRef[]; // только у ответов ассистента на основе базы документов
  webSources?: WebSource[]; // источники из веб-поиска (онлайн-режим)
  images?: string[]; // base64 прикреплённых изображений (зрение, qwen3-vl)
  // Ответ строился на данных из интернета (152-ФЗ: прозрачность видна и в старом
  // диалоге). Поле опциональное — сохранённые ранее диалоги читаются как офлайновые.
  online?: boolean;
}

// Сообщение для Ollama: role/content (+ опц. images у vision-запросов).
export type ApiMsg = { role: string; content: string; images?: string[] };

// Модель + поддержка рассуждений, зрения и вызова инструментов (из list_models).
export interface ModelInfo {
  name: string;
  thinking: boolean;
  vision: boolean;
  tools: boolean;
}

// Управление моделями (M1–M4): локальное состояние модели набора.
export interface ModelState {
  tag: string;
  role: string;
  title: string;
  required: boolean;
  installed: boolean;
  size?: number;
  digest?: string;
  modified_at?: string;
}

// Проекты (как в Claude): у каждого свои чаты и своя база знаний (документы).
export interface Project {
  id: string;
  name: string;
  instructions: string;
  created_at: number;
  updated_at: number;
}

// Карточка диалога для боковой панели и полный диалог из файла.
export interface ConversationMeta {
  id: string;
  title: string;
  updated_at: number;
  project_id?: string | null;
}
export interface Conversation {
  id: string;
  title: string;
  updated_at: number;
  project_id?: string | null;
  messages: Message[];
}

// Сведения о железе (из Rust-команды detect_hardware).
export interface HardwareInfo {
  ram_gb: number;
  cpu_cores: number;
  vram_gb: number | null; // всего (класс железа)
  vram_free_gb: number | null; // свободно сейчас (честная доступность)
  vram_source: string;
  tier: "green" | "yellow" | "red";
}

// Оценка модели набора против текущего железа — ДО установки (assess_models).
export interface ModelAssessment {
  tag: string;
  role: string;
  title: string;
  required: boolean;
  installed: boolean;
  size_gb: number; // фактический размер (установлена) либо приблизительный
  approx: boolean; // размер приблизительный (модель ещё не установлена)
  verdict: "ok" | "tight" | "no"; // потянет ли машина
  limiting: string; // "ram" | "vram"
}

// Найденный носитель с каталогом моделей Ollama (find_model_sources).
export interface ModelSource {
  path: string;
  removable: boolean;
  models: number;
  total_gb: number;
}

// Итог одной проверки диагностики (run_diagnostics).
export interface DiagCheck {
  id: string;
  title: string;
  status: string; // "ok" | "warn" | "fail"
  detail: string;
}

// Запись журнала исходящих обращений (зеркало OutboundLogEntry в lib.rs).
export interface OutboundLogEntry {
  ts: number;
  host: string;
  tool: string;
  query: string;
}

// Контекст работы с документами: карточка «Документы» в настройках (общая база,
// projectId=null) или раздел «Знания проекта» на экране проекта. Одна логика
// (documents.ts) обслуживает оба места через этот объект.
export interface DocCtx {
  projectId: string | null; // null = вне проектов (общая база быстрых чатов)
  listEl: HTMLElement;
  progressEl: HTMLElement;
  fillEl: HTMLElement;
  labelEl: HTMLElement;
  addBtn: HTMLButtonElement;
  flashTimer: number | null; // таймер авто-скрытия итоговой надписи (чиним гонку)
  // Счётчик операций виджета прогресса (аналог myGen в send): канал и результат
  // команды — разные пути IPC, запоздавший progress иначе затирал бы итоговую
  // надпись и снимал её таймер авто-скрытия (панель «залипала»).
  opGen: number;
}
