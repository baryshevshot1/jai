import { invoke, Channel } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
// Обновления приложения: проверка выпуска на GitHub — ТОЛЬКО по явной кнопке
// пользователя (автопроверок нет, офлайн-ядро не трогает сеть).
import { check, type Update } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";
import { getVersion } from "@tauri-apps/api/app";
import { renderMarkdown } from "./markdown";
import "katex/dist/katex.min.css";
import "highlight.js/styles/atom-one-dark.css";
// Шрифты — локально (бандлятся Vite), без сети: Inter (UI), Fraunces (бренд), JetBrains Mono (код).
import "@fontsource/inter/400.css";
import "@fontsource/inter/500.css";
import "@fontsource/inter/600.css";
import "@fontsource/inter/700.css";
import "@fontsource/fraunces/500.css";
import "@fontsource/fraunces/600.css";
import "@fontsource/jetbrains-mono/400.css";
import "@fontsource/jetbrains-mono/500.css";

// Модель по умолчанию. Выбор модели из набора — отдельный шаг (выпадающий список).
// Текущая выбранная модель (заполняется из списка установленных).
let selectedModel = "";

// Системная подсказка — задаёт деловой тон ассистента и запрещает эмодзи.
const SYSTEM = {
  role: "system" as const,
  content:
    "Ты — деловой ассистент. Отвечай по-русски, профессионально и по существу. " +
    "Не используй эмодзи и смайлики.",
};

// Калибровка контекста. На токенайзере Qwen русский текст — это ~2 символа/токен
// (раньше в коде было заложено оптимистичное «3», из-за чего бюджеты завышались и
// со второго-третьего хода Ollama молча отбрасывала начало истории). Считаем по 2.
const NUM_CTX_DEFAULT = 8192; // потолок контекста (CLAUDE.md); при нехватке памяти снижается
const CHARS_PER_TOKEN = 2; // русский, консервативно
const ANSWER_RESERVE_CHARS = 3000; // ~1500 токенов оставляем под сам ответ модели
// Полный бюджет промпта в символах: система + история + контекст из базы + вопрос.
// Всё, что сверх — отбрасываем сами (управляемо), не отдавая на молчаливую обрезку Ollama.
const PROMPT_CHAR_BUDGET = NUM_CTX_DEFAULT * CHARS_PER_TOKEN - ANSWER_RESERVE_CHARS;

// Прикреплённый документ (Фаза A). text уже усечён под бюджет контекста
// (≈2 симв/токен: 8000 симв ≈ 4000 токенов — половина num_ctx, остаток под систему/вопрос/ответ).
const DOC_CHAR_BUDGET = 8000;

// Картинки (base64) огромны и быстро переполняют num_ctx. В контекст берём изображения
// только у последних N ходов пользователя — свежий вопрос про картинку работает,
// а старые кадры не копятся в каждом запросе.
const IMAGE_HISTORY_TURNS = 2;

// Документ, привязанный к сообщению пользователя. text — уже усечённый под бюджет
// фрагмент (его «видит» модель); chars — полный размер исходника (для подписи).
interface DocAttachment {
  name: string;
  ext: string;
  text: string;
  chars: number;
  truncated: boolean;
}

// «Ожидающий» документ: выбран в композере, но ещё не отправлен. При отправке
// он привязывается к сообщению (Message.doc) и поле ввода очищается.
let pendingDoc: DocAttachment | null = null;

// «Ожидающее» изображение (зрение): base64 выбранной картинки до отправки.
// Одно изображение за сообщение — бюджет-безопасно для num_ctx.
let pendingImage: string | null = null;

// ── База документов (Фаза B5): RAG-поиск перед ответом ───────────────────────
// Сколько фрагментов искать и бюджет их суммарного объёма в контексте. Бюджет —
// эволюция Фазы A: место в num_ctx 8192 теперь занимают найденные фрагменты, плюс
// при активном поиске урезаем глубину истории, чтобы ответ не обрывался.
const RAG_TOP_K = 6;
const CONTEXT_CHAR_BUDGET = 5000; // ~2500 токенов (2 симв/ток) — с запасом под систему/вопрос/ответ
// Порог релевантности: vec0 отдаёт косинусную дистанцию (1 − cos), меньше = ближе.
// Фрагменты дальше порога считаем нерелевантными и в контекст не берём — иначе на
// любой вопрос (даже «привет») подмешивались бы top-6 и без нужды резалась история.
const RAG_MAX_DISTANCE = 0.6;

// Источник ответа (документ + № фрагмента) — для показа под ответом и истории.
interface SourceRef {
  filename: string;
  chunk_index: number;
}
// Найденный фрагмент из Rust (search_documents).
interface RetrievedChunk {
  text: string;
  filename: string;
  chunk_index: number;
  page: number | null;
  distance: number;
}
// Карточка документа базы (list_documents).
interface DocumentMeta {
  id: number;
  filename: string;
  ext: string;
  added_at: number;
  char_count: number;
  chunk_count: number;
}
// Прогресс индексации (Channel из index_document).
interface IndexProgress {
  phase: string;
  current: number;
  total: number;
}
// Прогресс установки модели (Channel из pull_model). Итог (успех/отмена) приходит
// РЕЗУЛЬТАТОМ команды pull_model — по нему честно отличаем «установлено» от отмены.
interface PullEvent {
  type: "progress";
  status: string;
  completed: number;
  total: number;
}
type PullOutcome = "done" | "cancelled";

// Число документов в базе: >0 → перед ответом ищем релевантные фрагменты.
let docsCount = 0;
// Установлена ли модель эмбеддингов (без неё индексация/поиск невозможны).
let embeddingReady = false;
// Тег модели, которую сейчас устанавливаем (null — установка не идёт): единый гейт
// от параллельных установок для всех трёх поверхностей + признак для карточки bge-m3.
let pullingTag: string | null = null;

// Документы-источники, уже показанные в строке «Источники» в текущем диалоге.
// Каждый документ упоминаем один раз — дальше не повторяем заметку под ответами.
const shownSourceFiles = new Set<string>();

// Модели, про которые уже показано примечание плана (напр., «тесно в свободной
// видеопамяти») — раз на модель за сессию, чтобы плашка не спамила каждый запрос.
const vramNotedModels = new Set<string>();

// Источник из веб-поиска (онлайн-режим): заголовок + ссылка (см. WebSource в tools.rs).
interface WebSource {
  title: string;
  url: string;
}

// События из Rust (см. ChatEvent в lib.rs). status/sources — только онлайн-слой.
type ChatEvent =
  | { type: "chunk"; content: string }
  | { type: "thinking"; content: string }
  | { type: "status"; content: string }
  | { type: "sources"; items: WebSource[] }
  | { type: "notice"; content: string }
  | { type: "done" };

type Role = "user" | "assistant";
interface Message {
  role: Role;
  content: string;
  doc?: DocAttachment; // только у реплик пользователя, к которым приложен файл
  sources?: SourceRef[]; // только у ответов ассистента на основе базы документов
  webSources?: WebSource[]; // источники из веб-поиска (онлайн-режим)
  images?: string[]; // base64 прикреплённых изображений (зрение, qwen3-vl)
}

// Сообщение для Ollama: role/content (+ опц. images у vision-запросов).
type ApiMsg = { role: string; content: string; images?: string[] };

// Модель + поддержка рассуждений, зрения и вызова инструментов (из list_models).
interface ModelInfo {
  name: string;
  thinking: boolean;
  vision: boolean;
  tools: boolean;
}
// Какие модели поддерживают «Размышления» / «Зрение» / «Инструменты» (имя → bool).
const thinkingByModel = new Map<string, boolean>();
const visionByModel = new Map<string, boolean>();
const toolsByModel = new Map<string, boolean>();

// Управление моделями (M1–M4): локальное состояние модели набора.
interface ModelState {
  tag: string;
  role: string;
  title: string;
  required: boolean;
  installed: boolean;
  size?: number;
  digest?: string;
  modified_at?: string;
}
// Статус обновления (M2): tag → "current" | "update" | "not_installed" | "error".
const updateByTag = new Map<string, string>();
let modelStates: ModelState[] = [];

// История ОТКРЫТОГО диалога (без системного сообщения — его добавляем при отправке).
const history: Message[] = [];

// id текущего диалога (файл appDataDir/conversations/<id>.json).
let currentId = "";
// Проект ОТКРЫТОГО чата: null = быстрый чат вне проектов. Определяет базу знаний
// (RAG) и инструкции, а также куда сохраняется диалог.
let currentProjectId: string | null = null;

// Кэш списка диалогов и текущий фильтр поиска (для отрисовки боковой панели).
let convMetas: ConversationMeta[] = [];
let convFilter = "";

// Проекты (как в Claude): у каждого свои чаты и своя база знаний (документы).
interface Project {
  id: string;
  name: string;
  instructions: string;
  created_at: number;
  updated_at: number;
}
let projects: Project[] = [];
let viewingProjectId: string | null = null; // проект, чей экран открыт (null = не открыт)

// Карточка диалога для боковой панели и полный диалог из файла.
interface ConversationMeta {
  id: string;
  title: string;
  updated_at: number;
  project_id?: string | null;
}
interface Conversation {
  id: string;
  title: string;
  updated_at: number;
  project_id?: string | null;
  messages: Message[];
}

// Счётчик «поколений»: позволяет кнопке «Стоп» игнорировать поздние кусочки.
let generation = 0;
let streaming = false;
// Дочистка активного стрима при «Стоп»: убрать индикатор «Думаю…», заморозить
// рассуждение и сохранить уже полученный частичный ответ в историю. Устанавливается
// в send() на время запроса, вызывается один раз (stop или нормальное завершение).
let activeStopCleanup: (() => void) | null = null;
// Следовать за ответом только если пользователь у низа ленты (иначе не мешаем читать).
let autoScroll = true;

// Режим рассуждений (тумблер). По умолчанию включён; выбор хранится в localStorage.
let thinkEnabled = true;

// Онлайн-режим (агентный, tool calling). По умолчанию ВЫКЛЮЧЕН (офлайн-первичность,
// 152-ФЗ). Включается явным тумблером; состояние хранится в settings.json (online_mode).
let onlineMode = false;

let messagesEl: HTMLElement;
let feedEl: HTMLElement;
let emptyStateEl: HTMLElement;
let inputEl: HTMLTextAreaElement;
let sendBtn: HTMLButtonElement;
let stopBtn: HTMLButtonElement;
let modelSelectEl: HTMLSelectElement;
let statusEl: HTMLElement;
let refreshBtn: HTMLButtonElement;
let hwBarEl: HTMLElement;
let convListEl: HTMLElement;
let newChatBtn: HTMLButtonElement;
let thinkToggleEl: HTMLButtonElement;
// Онлайн-режим (агентный): тумблер в композере, индикатор в шапке, поля и журнал в настройках.
let onlineToggleEl: HTMLButtonElement;
let onlineBadgeEl: HTMLElement;
let onlineStateTextEl: HTMLElement;
let onlineMasterToggleEl: HTMLButtonElement;
let wsProviderEl: HTMLInputElement;
let wsUrlEl: HTMLInputElement;
let wsKeyEl: HTMLInputElement;
let outboundLogEl: HTMLElement;
let outboundRefreshBtn: HTMLButtonElement;
let outboundClearBtn: HTMLButtonElement;
let onlineStatusEl: HTMLElement;
let themeBtn: HTMLButtonElement;
let settingsBtn: HTMLButtonElement;
let settingsView: HTMLElement;
let settingsBackBtn: HTMLButtonElement;
let composerWrapEl: HTMLElement;
let appEl: HTMLElement;
let sidebarResizer: HTMLElement;
let sidebarToggleBtn: HTMLButtonElement;
let convSearchEl: HTMLInputElement;
let checkBtn: HTMLButtonElement;
let clearBtn: HTMLButtonElement;
let attachBtn: HTMLButtonElement;
let docChipEl: HTMLElement;
let docChipBadgeEl: HTMLElement;
let docChipNameEl: HTMLElement;
let docRemoveBtn: HTMLButtonElement;
let imageBtn: HTMLButtonElement;
let imgChipEl: HTMLElement;
let imgChipThumb: HTMLImageElement;
let imgOcrBtn: HTMLButtonElement;
let imgRemoveBtn: HTMLButtonElement;
let tabChatsBtn: HTMLButtonElement;
let tabDocsBtn: HTMLButtonElement;
let paneChatsEl: HTMLElement;
let paneDocsEl: HTMLElement;
let addDocBtn: HTMLButtonElement;
let docListEl: HTMLElement;
let docStatusEl: HTMLElement;
let docStatusTextEl: HTMLElement;
let installEmbedBtn: HTMLButtonElement;
let installLocalBtn: HTMLButtonElement;
let pullCancelBtn: HTMLButtonElement;
let epModelsEl: HTMLElement;
let epEngineEl: HTMLElement;
let epSetModelsBtn: HTMLButtonElement;
let epSetEngineBtn: HTMLButtonElement;
let epResetBtn: HTMLButtonElement;
let settingsStatusEl: HTMLElement;
let modelListEl: HTMLElement;
let checkUpdatesBtn: HTMLButtonElement;
let installFromDiskBtn: HTMLButtonElement;
let modelsStatusEl: HTMLElement;
let modelProgressEl: HTMLElement;
let modelProgressFill: HTMLElement;
let modelProgressLabel: HTMLElement;
let modelPullCancelBtn: HTMLButtonElement;
let diagRunBtn: HTMLButtonElement;
let diagListEl: HTMLElement;
let appUpdateCheckBtn: HTMLButtonElement;
let appUpdateDiskBtn: HTMLButtonElement;
let appUpdateInfoEl: HTMLElement;
let appUpdateProgressEl: HTMLElement;
let appUpdateProgressFill: HTMLElement;
let appUpdateProgressLabel: HTMLElement;
let appUpdateStatusEl: HTMLElement;
let appVersionEl: HTMLElement;
let indexProgressEl: HTMLElement;
let indexProgressFill: HTMLElement;
let indexProgressLabel: HTMLElement;
// Проекты: боковая панель, экран проекта, знания и чаты проекта.
let projectListEl: HTMLElement;
let newProjectBtn: HTMLButtonElement;
let projectView: HTMLElement;
let projectBackBtn: HTMLButtonElement;
let projectNameInput: HTMLInputElement;
let projectDeleteBtn: HTMLButtonElement;
let projectInstructionsEl: HTMLTextAreaElement;
let projectStatusEl: HTMLElement;
let projectAddDocBtn: HTMLButtonElement;
let projectDocListEl: HTMLElement;
let projectChatListEl: HTMLElement;
let projectIndexProgressEl: HTMLElement;
let projectIndexProgressFill: HTMLElement;
let projectIndexProgressLabel: HTMLElement;
let chatProjectChip: HTMLButtonElement;
let chatProjectChipName: HTMLElement;

// Формат файла → подпись бейджа и CSS-класс цвета. Неизвестное — нейтральный «ФАЙЛ».
function fileFormat(ext: string): { label: string; cls: string } {
  switch (ext.toLowerCase()) {
    case "pdf":
      return { label: "PDF", cls: "fmt--pdf" };
    case "docx":
    case "doc":
      return { label: "DOCX", cls: "fmt--docx" };
    case "md":
      return { label: "MD", cls: "fmt--md" };
    case "txt":
      return { label: "TXT", cls: "fmt--txt" };
    default:
      return { label: "ФАЙЛ", cls: "fmt--txt" };
  }
}

// Подпись под именем файла: полный размер либо отметка усечённого фрагмента.
function docSubline(doc: DocAttachment): string {
  const n = doc.chars.toLocaleString("ru");
  const unit = plural(doc.chars, "символ", "символа", "символов");
  return doc.truncated ? `Фрагмент · из ${n} ${unit}` : `${n} ${unit}`;
}

// Карточка прикреплённого файла внутри пузыря сообщения (иконка-бейдг + имя + размер).
function buildDocCard(doc: DocAttachment): HTMLElement {
  const fmt = fileFormat(doc.ext);
  const card = document.createElement("div");
  card.className = "msg-doc";
  card.title = `${doc.name} · ${fmt.label}`;

  const badge = document.createElement("span");
  badge.className = `fmt-badge ${fmt.cls}`;
  badge.textContent = fmt.label;
  card.appendChild(badge);

  const info = document.createElement("div");
  info.className = "msg-doc__info";
  const name = document.createElement("span");
  name.className = "msg-doc__name";
  name.textContent = doc.name;
  const sub = document.createElement("span");
  sub.className = "msg-doc__sub";
  sub.textContent = docSubline(doc);
  info.append(name, sub);
  card.appendChild(info);
  return card;
}

// Создаёт «обмен» (turn) и возвращает контейнер для текста (для дозаписи):
// пользователь — справа в градиент-пузыре; ассистент — слева с аватаром «j».
function addBubble(
  role: Role,
  text: string,
  doc?: DocAttachment,
  sources?: SourceRef[],
  images?: string[],
  webSources?: WebSource[],
): HTMLElement {
  const turn = document.createElement("div");
  let body: HTMLElement;
  if (role === "user") {
    turn.className = "turn me";
    if (doc) turn.appendChild(buildDocCard(doc)); // карточка файла — над текстом запроса
    if (images) {
      for (const b64 of images) {
        const img = document.createElement("img");
        img.className = "msg-img";
        img.src = imageDataUrl(b64);
        img.alt = "Прикреплённое изображение";
        turn.appendChild(img); // миниатюра — над текстом запроса
      }
    }
    body = document.createElement("div");
    body.className = "user-msg";
    body.textContent = text; // реплику пользователя — простым текстом
    turn.appendChild(body);
  } else {
    turn.className = "turn ai";
    body = document.createElement("div");
    body.className = "msg";
    body.innerHTML = renderMarkdown(text); // ответ — как Markdown/формулы, без аватара/подписи
    turn.appendChild(body);
    if (sources && sources.length) renderSources(turn, sources); // источники из базы
    if (webSources && webSources.length) renderWebSources(turn, webSources); // из интернета
  }
  messagesEl.appendChild(turn);
  refreshEmptyState();
  scrollToBottom();
  return body;
}

// Строит ответ ассистента: индикатор «думаю» (точки), переливающееся
// «Рассуждение» с текстом (без рамки/коллапса) и контейнер ответа.
function addAssistantTurn() {
  const turn = document.createElement("div");
  turn.className = "turn ai";

  const thinking = document.createElement("div");
  thinking.className = "thinking";
  thinking.innerHTML =
    '<span class="dots"><i></i><i></i><i></i></span><span>Думаю над ответом</span>';
  turn.appendChild(thinking);

  const reason = document.createElement("div");
  reason.className = "reason";
  reason.hidden = true;
  const reasonWord = document.createElement("div");
  reasonWord.className = "reason-word shimmer";
  reasonWord.textContent = "Рассуждение";
  reason.appendChild(reasonWord);
  const rbody = document.createElement("div");
  rbody.className = "rbody clamp"; // по умолчанию обрезано до 3 строк
  reason.appendChild(rbody);
  const toggle = document.createElement("button");
  toggle.type = "button";
  toggle.className = "reason-toggle";
  toggle.hidden = true;
  toggle.textContent = "Показать больше";
  reason.appendChild(toggle);
  turn.appendChild(reason);

  const msg = document.createElement("div");
  msg.className = "msg";
  turn.appendChild(msg);

  messagesEl.appendChild(turn);
  refreshEmptyState();
  scrollToBottom();
  return { turn, thinking, reason, reasonWord, rbody, toggle, msg };
}

function addError(text: string) {
  const row = document.createElement("div");
  row.className = "err";
  row.textContent = text;
  messagesEl.appendChild(row);
  refreshEmptyState();
  scrollToBottom();
}

// Нейтральное уведомление в ленте (не ошибка) — напр. предупреждение об усечении.
function addNotice(text: string) {
  const row = document.createElement("div");
  row.className = "notice";
  row.textContent = text;
  messagesEl.appendChild(row);
  refreshEmptyState();
  scrollToBottom();
}

function scrollToBottom() {
  if (!autoScroll) return; // прокрутил вверх — не тянем обратно вниз
  feedEl.scrollTop = feedEl.scrollHeight;
}

// Приветствие видно, только когда в открытом диалоге нет сообщений.
function refreshEmptyState() {
  emptyStateEl.hidden = messagesEl.children.length > 0;
}

function setStreaming(on: boolean) {
  streaming = on;
  sendBtn.hidden = on;
  stopBtn.hidden = !on;
  inputEl.disabled = on;
  if (!on && selectedModel) inputEl.focus(); // вернуть фокус в поле после ответа
}

// Собирает массив сообщений для Ollama: система + история (+ контекст из базы),
// уложенная в бюджет символов PROMPT_CHAR_BUDGET. Всегда сохраняем систему, контекст
// из базы и ТЕКУЩИЙ ход; предыдущие ходы добавляем от новых к старым, пока помещаются
// (старые отбрасываем сами — управляемо, а не отдаём на молчаливую обрезку Ollama, из-за
// которой модель «забывала» начало диалога и приложенные файлы). Реплику с документом
// разворачиваем в текст «документ + вопрос»; объекты doc/sources в запрос не уходят.
function buildApiMessages(contextMsg: Message | null): ApiMsg[] {
  const n = history.length;

  // Картинки оставляем только у последних IMAGE_HISTORY_TURNS ходов с изображениями.
  const keepImages = new Set<number>();
  for (let i = n - 1, kept = 0; i >= 0 && kept < IMAGE_HISTORY_TURNS; i--) {
    if (history[i].images && history[i].images!.length) {
      keepImages.add(i);
      kept++;
    }
  }

  // Превращает сообщение истории в сообщение для Ollama (с разворотом документа).
  const materialize = (m: Message, i: number): ApiMsg => {
    let content: string;
    if (m.role === "user" && m.doc) {
      // Документ — справочный материал к ходу. Не приказываем «отвечай только по нему»
      // и не просим упоминать в каждом ответе: модель обращается к файлу по релевантности.
      content =
        `[Прикреплён документ «${m.doc.name}» — справочный материал, ` +
        `обращайся к нему, когда это относится к вопросу]\n\n` +
        `${m.doc.text}\n\n———\n\n${m.content}`;
    } else {
      content = m.content;
    }
    const item: ApiMsg = { role: m.role, content };
    if (m.role === "user" && m.images && m.images.length && keepImages.has(i)) {
      item.images = m.images;
    }
    return item;
  };

  const lastIdx = n - 1;
  const current = lastIdx >= 0 ? materialize(history[lastIdx], lastIdx) : null;
  const ctxItem: ApiMsg | null = contextMsg
    ? { role: contextMsg.role, content: contextMsg.content }
    : null;

  // Остаток бюджета под предыдущие ходы: вычитаем систему, контекст и текущий ход
  // (их сохраняем всегда). Картинки в бюджет символов не считаем — они в поле images.
  let budget = PROMPT_CHAR_BUDGET - SYSTEM.content.length;
  if (ctxItem) budget -= ctxItem.content.length;
  if (current) budget -= current.content.length;

  const older: ApiMsg[] = [];
  for (let i = lastIdx - 1; i >= 0; i--) {
    const item = materialize(history[i], i);
    if (item.content.length > budget) break; // дальше не помещается — старое отбрасываем
    budget -= item.content.length;
    older.unshift(item);
  }

  const out: ApiMsg[] = [SYSTEM, ...older];
  if (ctxItem) out.push(ctxItem); // фрагменты из базы — прямо перед текущим вопросом
  if (current) out.push(current);
  return out;
}

async function send() {
  const text = inputEl.value.trim();
  if (!text || streaming || !selectedModel) return;

  inputEl.value = "";
  autoGrow();
  // Документ и/или изображение из композера привязываем к ЭТОМУ сообщению и сразу
  // убираем из поля ввода — они «уехали» вместе с вопросом.
  const doc = pendingDoc ?? undefined;
  const images = pendingImage ? [pendingImage] : undefined;
  clearPendingDoc();
  clearPendingImage();
  history.push({
    role: "user",
    content: text,
    ...(doc ? { doc } : {}),
    ...(images ? { images } : {}),
  });
  addBubble("user", text, doc, undefined, images);
  persist(); // вопрос (с файлом/картинкой) сохраняется сразу
  autoScroll = true; // при отправке снова следуем за ответом

  const myGen = ++generation;
  setStreaming(true);

  // Ответ ассистента: индикатор «думаю», переливающееся рассуждение, текст.
  const ui = addAssistantTurn();
  let answer = "";
  let reasoning = "";
  let reasonExpanded = false;
  const startTs = Date.now();

  // Кнопка «Показать больше/меньше»: видна, только если рассуждение длиннее 3 строк.
  const syncToggle = () => {
    if (reasonExpanded) {
      ui.toggle.hidden = false;
      ui.toggle.textContent = "Показать меньше";
    } else {
      const overflows = ui.rbody.scrollHeight > ui.rbody.clientHeight + 1;
      ui.toggle.hidden = !overflows;
      ui.toggle.textContent = "Показать больше";
    }
  };
  ui.toggle.addEventListener("click", () => {
    reasonExpanded = !reasonExpanded;
    ui.rbody.classList.toggle("clamp", !reasonExpanded);
    syncToggle();
  });

  // Когда пошёл ответ — замораживаем «Рассуждение» в статичную кликабельную
  // подпись «Рассуждение · N сек» и СВОРАЧИВАЕМ текст рассуждения (ответ — главный).
  const freezeReason = (withTime: boolean) => {
    if (!reasoning || !ui.reasonWord.classList.contains("shimmer")) return;
    ui.reasonWord.classList.remove("shimmer");
    ui.reasonWord.classList.add("reason-done"); // кликабельно: раскрыть/скрыть
    const sec = Math.max(1, Math.round((Date.now() - startTs) / 1000));
    ui.reasonWord.textContent = withTime ? `Рассуждение · ${sec} сек` : "Рассуждение";
    ui.rbody.hidden = true;
    ui.toggle.hidden = true;
    reasonExpanded = false;
    ui.rbody.classList.add("clamp");
  };

  // Клик по застывшей подписи — показать/скрыть текст рассуждения.
  ui.reasonWord.addEventListener("click", () => {
    if (ui.reasonWord.classList.contains("shimmer")) return; // ещё думает — не трогаем
    const show = ui.rbody.hidden;
    ui.rbody.hidden = !show;
    if (show) syncToggle();
    else ui.toggle.hidden = true;
  });

  // Безопасная отрисовка ответа: пробуем Markdown, при ошибке — полный текст.
  const renderAnswer = (text: string) => {
    try {
      ui.msg.innerHTML = renderMarkdown(text);
    } catch {
      ui.msg.textContent = text; // форматирование упало — показываем хотя бы весь текст
    }
  };

  const webSources: WebSource[] = []; // источники из веб-поиска (онлайн-режим)
  const onEvent = new Channel<ChatEvent>();
  onEvent.onmessage = (msg) => {
    if (myGen !== generation) return; // нажали «Стоп» — игнорируем хвост
    if (msg.type === "status") {
      // Агентный цикл сообщает стадию («Ищу в интернете…») — показываем в индикаторе.
      const lbl = ui.thinking.querySelector("span:last-child");
      if (lbl) lbl.textContent = msg.content;
    } else if (msg.type === "sources") {
      // Источники веб-поиска: копим без дублей по URL, рисуем под финальным ответом.
      for (const s of msg.items) {
        if (!webSources.some((x) => x.url === s.url)) webSources.push(s);
      }
    } else if (msg.type === "notice") {
      // Служебное уведомление от бэкенда (например, переполнение контекста).
      addNotice(msg.content);
    } else if (msg.type === "thinking") {
      reasoning += msg.content;
      ui.thinking.remove(); // индикатор теперь — переливающееся «Рассуждение»
      ui.reason.hidden = false;
      ui.rbody.textContent = reasoning;
      scrollToBottom();
    } else if (msg.type === "chunk") {
      answer += msg.content;
      if (answer) {
        ui.thinking.remove(); // пошёл ответ — убираем «Думаю…»
        freezeReason(true);
        ui.msg.textContent = answer; // живая печать ПРОСТЫМ текстом — дёшево, ничего не виснет
      }
      scrollToBottom();
    }
    // финал — по результату команды ниже (авторитетный полный ответ)
  };

  // RAG: при непустой базе ищем релевантные фрагменты ДО обращения к модели и
  // вставляем их как контекст. Поиск не должен ронять чат — при сбое идём обычным.
  let contextMsg: Message | null = null;
  let sources: SourceRef[] = [];

  // «Стоп» в любой момент запроса (поиск по базе, оценка памяти, генерация): убрать
  // индикатор «Думаю…», заморозить рассуждение и сохранить уже полученный частичный
  // ответ в историю — иначе «Думаю…» висит вечно, а ответ теряется (модель потом «не
  // помнит» свою реплику). Замыкание видит актуальные answer/reasoning/sources.
  activeStopCleanup = () => {
    ui.thinking.remove();
    freezeReason(true);
    if (reasoning) ui.rbody.textContent = reasoning;
    if (answer.trim()) {
      renderAnswer(answer);
      if (sources.length) renderSources(ui.turn, sources);
      if (webSources.length) renderWebSources(ui.turn, webSources);
      history.push({
        role: "assistant",
        content: answer,
        ...(sources.length ? { sources } : {}),
        ...(webSources.length ? { webSources } : {}),
      });
      persist();
    } else if (!reasoning.trim()) {
      ui.turn.remove(); // совсем пусто — убираем ход
    }
    scrollToBottom();
  };

  if (docsCount > 0) {
    try {
      // Поиск в базе ПРОЕКТА открытого чата (или общей, вне проектов — currentProjectId=null).
      const retrieved = await invoke<RetrievedChunk[]>("search_documents", {
        query: text,
        k: RAG_TOP_K,
        projectId: currentProjectId,
      });
      if (myGen !== generation) return; // остановили во время поиска
      // Берём только релевантные фрагменты (порог по косинусной дистанции). Если ни один
      // не прошёл — вопрос не про документы: идём обычным путём, историю не режем.
      const relevant = retrieved.filter((r) => r.distance <= RAG_MAX_DISTANCE);
      if (relevant.length) {
        const built = buildContext(relevant);
        contextMsg = built.contextMsg;
        sources = built.sources;
      }
    } catch (e) {
      if (myGen !== generation) return;
      addNotice(`Поиск по документам недоступен: ${humanError(e)}`);
    }
  }

  // Контекст для модели: система + история (урезанная при RAG) + контекст из базы.
  // У реплик с приложенным файлом текст документа вшивается в ход (как в Фазе A).
  const messages = buildApiMessages(contextMsg);

  // Инструкции проекта (если чат внутри проекта) — системным сообщением сразу после
  // базовой системы. Задают роль и правила для всех чатов этого проекта (как в Claude).
  if (currentProjectId) {
    const proj = projects.find((p) => p.id === currentProjectId);
    const instr = proj?.instructions.trim();
    if (instr) messages.splice(1, 0, { role: "system", content: instr });
  }

  // Лестница смягчения (S2): ДО запуска оцениваем память по формуле. При нехватке —
  // снижаем контекст / подбираем модель полегче «вниз» / честно отказываем. Ручной
  // выбор не меняем: downscale действует только на этот запрос.
  let useModel = selectedModel;
  let useCtx: number | undefined;
  let downscaleNote: string | null = null;
  try {
    const plan = await invoke<{
      action: string;
      model: string;
      num_ctx: number;
      reason: string | null;
      original_model: string;
      note: string | null;
    }>("plan_inference", { model: selectedModel });
    if (myGen !== generation) return;
    if (plan.action === "refuse") {
      activeStopCleanup = null; // отказ до старта — дочистка не нужна
      ui.thinking.remove();
      ui.turn.remove();
      addNotice(
        `Не запускаю «${plan.original_model}»: ${plan.reason}. ` +
          `Выберите модель полегче или освободите память.`,
      );
      setStreaming(false);
      return;
    }
    useModel = plan.model;
    useCtx = plan.num_ctx;
    // Честное примечание (напр., «тесно в свободной видеопамяти — будет медленнее»):
    // показываем один раз на модель за сессию, чтобы не спамить каждый запрос.
    if (plan.note && !vramNotedModels.has(plan.model)) {
      vramNotedModels.add(plan.model);
      addNotice(plan.note);
    }
    if (plan.action === "downscale" && plan.reason) {
      const cap = (s: string) => s.charAt(0).toUpperCase() + s.slice(1);
      downscaleNote =
        plan.model !== plan.original_model
          ? `Выполнено на «${plan.model}»: ${plan.reason}.`
          : `${cap(plan.reason)}.`;
    }
  } catch {
    if (myGen !== generation) return;
    // оценка недоступна (нет Ollama и т.п.) — не блокируем, идём как есть
  }

  // Онлайн-режим: при включённом тумблере и модели с поддержкой инструментов идём
  // агентным путём (tool calling: веб-поиск). Иначе — обычный офлайн-стрим, как и
  // раньше. Модель без `tools` в онлайне → честное уведомление, чат работает обычным.
  const useTools = onlineMode && (toolsByModel.get(useModel) ?? false);
  if (onlineMode && !useTools) {
    addNotice(
      `Модель «${useModel}» не умеет вызывать инструменты — веб-поиск недоступен. ` +
        `Отвечаю офлайн. Для онлайн-инструментов выберите модель с поддержкой инструментов (например, qwen3.5:9b).`,
    );
  }
  // Онлайн: усиливаем поиск — подмешиваем системную подсказку с текущей датой, чтобы
  // модель искала, когда вопрос требует данных, и строила вывод на основе найденного.
  // Вставка ТОЛЬКО при агентном пути — офлайн-промпт остаётся без изменений.
  if (useTools) {
    const today = new Date().toLocaleDateString("ru-RU", {
      day: "numeric",
      month: "long",
      year: "numeric",
    });
    messages.splice(1, 0, {
      role: "system",
      content:
        `Сегодня ${today}. Онлайн-режим включён, доступен инструмент web_search (поиск в интернете). ` +
        `ВАЖНО: твои внутренние знания устарели и не содержат текущих данных реального мира. Поэтому для ` +
        `любого вопроса, ответ на который зависит от фактов — цены, курсы валют, новости, законы, даты, ` +
        `характеристики товаров и оборудования, статистика, события, «что/как/сколько/где/когда сейчас» — ` +
        `сначала обязательно вызови web_search по сути запроса. Один поиск возвращает до 20 источников — ` +
        `этого обычно достаточно для полного охвата; при необходимости охватить другой аспект сделай ещё ` +
        `один поиск (не больше двух всего). Затем обработай и сопоставь ВСЕ найденные источники вместе и ` +
        `дай развёрнутый ответ с анализом и обоснованным выводом (а не перечень ссылок), опираясь на источники ` +
        `и отмечая расхождения, если они есть. Не отвечай по памяти на фактические вопросы. Без поиска отвечай ` +
        `только на просьбы написать или переформулировать текст, посчитать, либо объяснить общее устойчивое понятие.`,
    });
  }

  try {
    // Возвращённое значение — ПОЛНЫЙ текст ответа (без гонок с доставкой канала).
    const full = await invoke<string>(useTools ? "agentic_chat" : "chat_stream", {
      model: useModel,
      messages,
      // think:true шлём ТОЛЬКО моделям, которые это поддерживают (иначе Ollama
      // вернёт ошибку «не умеет размышлять»).
      think: thinkEnabled && (thinkingByModel.get(useModel) ?? false),
      numCtx: useCtx, // рычаг смягчения (Rust: undefined → 8192)
      onEvent,
    });
    if (myGen === generation) {
      activeStopCleanup = null; // нормально завершились — дочистка «Стоп» не нужна
      ui.thinking.remove();
      freezeReason(true);
      if (reasoning) ui.rbody.textContent = reasoning; // готов, раскрывается по клику
      answer = full; // авторитетный полный ответ
      if (answer.trim()) {
        renderAnswer(answer); // финальное форматирование один раз
        if (sources.length) renderSources(ui.turn, sources); // из каких документов взято
        if (webSources.length) renderWebSources(ui.turn, webSources); // из интернета
        history.push({
          role: "assistant",
          content: answer,
          ...(sources.length ? { sources } : {}),
          ...(webSources.length ? { webSources } : {}),
        });
        persist();
      } else if (!reasoning.trim()) {
        ui.turn.remove(); // совсем пусто — убираем
      }
      // Пост-фактум: спокойно сообщаем, что подобрали модель/контекст полегче.
      if (downscaleNote && (answer.trim() || reasoning.trim())) addNotice(downscaleNote);
      scrollToBottom();
      setStreaming(false);
    }
  } catch (err) {
    if (myGen !== generation) return;
    activeStopCleanup = null; // завершились с ошибкой — дочистка «Стоп» не нужна
    ui.thinking.remove();
    if (!answer && !reasoning) ui.turn.remove();
    addError(String(err));
    setStreaming(false);
  }
}

function stop() {
  if (!streaming) return;
  generation++; // «отвязываем» текущий запрос — поздние кусочки игнорируются
  invoke("cancel_stream").catch(() => {}); // и реально останавливаем генерацию в Ollama
  // Дочистить UI и сохранить частичный ответ (иначе «Думаю…» зависает, ответ теряется).
  if (activeStopCleanup) {
    const cleanup = activeStopCleanup;
    activeStopCleanup = null;
    cleanup();
  }
  setStreaming(false);
}

function setComposerEnabled(on: boolean) {
  inputEl.disabled = !on;
  sendBtn.disabled = !on;
}

// ── Документы (Фаза A): прикрепление одного файла ────────────────────────────

// Чип над полем ввода в режиме загрузки (пока Rust извлекает текст).
function showDocChipLoading() {
  docChipBadgeEl.className = "fmt-badge fmt--txt";
  docChipBadgeEl.textContent = "…";
  docChipNameEl.textContent = "Читаю документ…";
  docChipEl.classList.add("doc-chip--loading");
  docRemoveBtn.hidden = true;
  docChipEl.hidden = false;
}

// Чип над полем ввода для готового документа (бейдж формата + имя + кнопка «убрать»).
function showDocChip(doc: DocAttachment) {
  const fmt = fileFormat(doc.ext);
  docChipBadgeEl.className = `fmt-badge ${fmt.cls}`;
  docChipBadgeEl.textContent = fmt.label;
  docChipNameEl.textContent = doc.name;
  docChipEl.title = `${doc.name} · ${docSubline(doc)}`;
  docChipEl.classList.remove("doc-chip--loading");
  docRemoveBtn.hidden = false;
  docChipEl.hidden = false;
}

function hideDocChip() {
  docChipEl.hidden = true;
  docChipEl.classList.remove("doc-chip--loading");
  docChipEl.removeAttribute("title");
}

// Полностью сбросить «ожидающий» документ и убрать чип из композера.
function clearPendingDoc() {
  pendingDoc = null;
  hideDocChip();
}

// «Прикрепить документ»: нативный диалог → извлечение текста в Rust → чип.
async function attachDocument() {
  let path: string | null;
  try {
    const sel = await open({
      multiple: false,
      filters: [{ name: "Документы", extensions: ["pdf", "docx", "txt", "md"] }],
    });
    path = typeof sel === "string" ? sel : null;
  } catch (e) {
    addError(`Не удалось открыть диалог: ${e}`);
    return;
  }
  if (!path) return; // отмена выбора

  attachBtn.disabled = true;
  showDocChipLoading();

  let doc: { name: string; ext: string; text: string; chars: number };
  try {
    doc = await invoke("extract_document", { path });
  } catch (e) {
    clearPendingDoc();
    attachBtn.disabled = false;
    addError(String(e));
    return;
  }

  // Бюджет контекста: большой документ не валим целиком — берём первую часть и предупреждаем.
  const truncated = doc.text.length > DOC_CHAR_BUDGET;
  const text = truncated ? doc.text.slice(0, DOC_CHAR_BUDGET) : doc.text;
  pendingDoc = { name: doc.name, ext: doc.ext, text, chars: doc.chars, truncated };
  showDocChip(pendingDoc);
  attachBtn.disabled = false;
  if (truncated) {
    addNotice(
      `Документ «${doc.name}» большой (${doc.chars.toLocaleString("ru")} символов). ` +
        `Использована первая часть (~${DOC_CHAR_BUDGET.toLocaleString("ru")} символов). ` +
        `Полная работа с большими документами появится в следующем этапе (поиск по документу).`,
    );
  }
  inputEl.focus();
}

function removeDocument() {
  clearPendingDoc();
  inputEl.focus();
}

// ── Зрение (qwen3-vl): прикрепление изображения, превью, OCR ─────────────────

// MIME изображения по сигнатуре base64 (для data-URL превью; точный тип не храним).
function imageMime(b64: string): string {
  if (b64.startsWith("/9j/")) return "image/jpeg";
  if (b64.startsWith("iVBORw0KG")) return "image/png";
  if (b64.startsWith("UklGR")) return "image/webp";
  if (b64.startsWith("R0lGOD")) return "image/gif";
  return "image/png";
}

function imageDataUrl(b64: string): string {
  return `data:${imageMime(b64)};base64,${b64}`;
}

function showImageChip(b64: string) {
  imgChipThumb.src = imageDataUrl(b64);
  imgChipEl.hidden = false;
}

function clearPendingImage() {
  pendingImage = null;
  imgChipEl.hidden = true;
  imgChipThumb.removeAttribute("src");
}

// Есть ли среди установленных моделей хоть одна с поддержкой зрения.
function anyVisionModel(): string | null {
  for (const [name, vision] of visionByModel) if (vision) return name;
  return null;
}

// «Прикрепить изображение»: диалог → чтение base64 в Rust → превью + гейт vision-модели.
async function attachImage() {
  let path: string | null;
  try {
    const sel = await open({
      multiple: false,
      filters: [{ name: "Изображения", extensions: ["png", "jpg", "jpeg", "webp", "gif"] }],
    });
    path = typeof sel === "string" ? sel : null;
  } catch (e) {
    addError(`Не удалось открыть диалог: ${e}`);
    return;
  }
  if (!path) return;

  let b64: string;
  try {
    b64 = await invoke<string>("read_image_base64", { path });
  } catch (e) {
    addError(String(e)); // формат/размер — понятная ошибка из Rust
    return;
  }
  pendingImage = b64;
  showImageChip(b64);
  ensureVisionModel(); // подобрать/переключить vision-модель или предложить установку
  inputEl.focus();
}

function removeImage() {
  clearPendingImage();
  inputEl.focus();
}

// Подбор vision-модели: текущая умеет — ок; иначе переключиться на установленную
// (с уведомлением) либо предложить установить qwen3-vl.
function ensureVisionModel() {
  if (visionByModel.get(selectedModel)) return; // текущая модель видит изображения
  const vis = anyVisionModel();
  if (vis) {
    selectedModel = vis;
    modelSelectEl.value = vis;
    updateThinkAvailability();
    invoke("set_setting", { key: "selected_model", value: vis }).catch(() => {});
    addNotice(`Для работы с изображением переключился на модель «${vis}».`);
  } else {
    offerInstallVision();
  }
}

// OCR: «Извлечь текст» — тот же путь зрения, но с готовым OCR-промптом на русском.
const OCR_PROMPT =
  "Распознай и извлеки весь текст с изображения дословно, сохраняя структуру " +
  "(абзацы, списки, таблицы по возможности). Выведи только извлечённый текст.";

function ocrImage() {
  if (!pendingImage || streaming || !selectedModel) return;
  inputEl.value = OCR_PROMPT;
  autoGrow();
  send(); // vision-запрос с OCR-промптом + прикреплённая картинка
}

// Нет vision-модели → предложить установить qwen3-vl через существующий pull_model.
// Лёгкий документированный вариант (см. CLAUDE.md и набор моделей на странице настроек).
const VISION_MODEL = "qwen3-vl:4b"; // лёгкий вариант для зрения/OCR

async function offerInstallVision() {
  if (pullingTag) {
    addNotice(`Дождитесь завершения установки «${pullingTag}» — затем можно ставить модель зрения.`);
    return;
  }
  const ok = await confirmModal(
    `Для работы с изображениями нужна модель зрения. Установить ${VISION_MODEL} (~3–4 ГБ)? Потребуется интернет.`,
    "Установить",
  );
  if (!ok) {
    addNotice(
      "Чтобы работать с изображениями, установите vision-модель (например, qwen3-vl) — " +
        "онлайн или с диска (вкладка «Документы» → локальная поставка).",
    );
    return;
  }
  const row = document.createElement("div");
  row.className = "notice";
  const label = document.createElement("span");
  label.textContent = `Установка ${VISION_MODEL}…`;
  const cancelBtn = document.createElement("button");
  cancelBtn.type = "button";
  cancelBtn.className = "notice-cancel";
  cancelBtn.textContent = "Отмена";
  cancelBtn.addEventListener("click", () => {
    cancelBtn.disabled = true;
    cancelActivePull();
  });
  row.append(label, cancelBtn);
  messagesEl.appendChild(row);
  refreshEmptyState();
  scrollToBottom();

  const outcome = await runPull(VISION_MODEL, {
    progress: (t) => {
      label.textContent = `Установка ${VISION_MODEL}: ${t}`;
      scrollToBottom();
    },
    done: () => {
      label.textContent = `Модель ${VISION_MODEL} установлена — можно работать с изображениями.`;
    },
    cancelled: () => {
      label.textContent = `Установка ${VISION_MODEL} отменена — можно вернуться к ней позже.`;
    },
    error: (m) => {
      row.className = "err";
      label.textContent = `Не удалось установить ${VISION_MODEL}: ${m}`;
    },
  });
  cancelBtn.remove();
  if (outcome === "done") {
    await loadModels();
    ensureVisionModel(); // теперь vision-модель есть → переключимся на неё
  }
}

// ── База документов: контекст (проект или «вне проектов») + список/добавление ──
// Одна логика обслуживает и вкладку «Документы» в сайдбаре (общие документы для
// быстрых чатов, projectId=null), и раздел «Знания проекта» на экране проекта.

interface DocCtx {
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
let sidebarDocCtx: DocCtx; // общие документы (сайдбар), projectId всегда null
let projectDocCtx: DocCtx; // знания проекта (экран проекта), projectId — текущий проект

function switchTab(tab: "chats" | "docs") {
  const docs = tab === "docs";
  paneChatsEl.hidden = docs;
  paneDocsEl.hidden = !docs;
  tabChatsBtn.classList.toggle("active", !docs);
  tabDocsBtn.classList.toggle("active", docs);
  if (docs) refreshDocuments(sidebarDocCtx); // на открытии вкладки — свежий список
}

// Тянет список документов контекста и (для сайдбара) статус модели эмбеддингов.
async function refreshDocuments(ctx: DocCtx) {
  try {
    embeddingReady = await invoke<boolean>("embedding_status");
  } catch {
    embeddingReady = false;
  }
  // Карточка установки bge-m3 — только в сайдбаре (общий статус модели поиска).
  if (ctx === sidebarDocCtx) {
    if (embeddingReady) {
      docStatusEl.hidden = true;
    } else if (pullingTag === null) {
      docStatusEl.hidden = false;
      docStatusTextEl.textContent =
        "Для поиска по документам нужна модель bge-m3. Скачайте из интернета или укажите локальную поставку (каталог моделей Ollama) — без терминала.";
      installEmbedBtn.hidden = false;
      installEmbedBtn.disabled = false;
      installEmbedBtn.textContent = "Скачать (~1.2 ГБ)";
      installLocalBtn.hidden = false;
      installLocalBtn.disabled = false;
    }
  }

  let docs: DocumentMeta[] = [];
  try {
    docs = await invoke<DocumentMeta[]>("list_documents", { projectId: ctx.projectId });
  } catch (e) {
    console.error("list_documents:", e);
  }
  renderDocList(docs, ctx);
}

function renderDocList(docs: DocumentMeta[], ctx: DocCtx) {
  ctx.listEl.innerHTML = "";
  if (docs.length === 0) {
    const empty = document.createElement("div");
    empty.className = "doc-empty";
    empty.textContent = !embeddingReady
      ? "Для документов нужна модель поиска bge-m3 (установите во вкладке «Документы»)."
      : ctx.projectId
        ? "В проекте пока нет документов. Добавьте — и чаты проекта будут искать по ним."
        : "База пуста. Добавьте документы — и спрашивайте по ним.";
    ctx.listEl.appendChild(empty);
    return;
  }
  for (const d of docs) {
    const fmt = fileFormat(d.ext);
    const item = document.createElement("div");
    item.className = "doc-item";

    const badge = document.createElement("span");
    badge.className = `fmt-badge ${fmt.cls}`;
    badge.textContent = fmt.label;
    item.appendChild(badge);

    const info = document.createElement("div");
    info.className = "doc-item__info";
    const name = document.createElement("span");
    name.className = "doc-item__name";
    name.textContent = d.filename;
    const sub = document.createElement("span");
    sub.className = "doc-item__sub";
    const frags = `${d.chunk_count} ${plural(d.chunk_count, "фрагмент", "фрагмента", "фрагментов")}`;
    sub.textContent = `${new Date(d.added_at).toLocaleDateString("ru")} · ${frags}`;
    info.append(name, sub);
    item.appendChild(info);

    const del = document.createElement("button");
    del.className = "doc-item__del";
    del.textContent = "×";
    del.title = "Удалить документ из базы";
    del.addEventListener("click", () => deleteDocument(d, ctx));
    item.appendChild(del);

    ctx.listEl.appendChild(item);
  }
}

function showIndexProgress(label: string, frac: number, ctx: DocCtx) {
  if (ctx.flashTimer !== null) {
    clearTimeout(ctx.flashTimer);
    ctx.flashTimer = null;
  }
  ctx.progressEl.hidden = false;
  ctx.fillEl.style.width = `${Math.round(frac * 100)}%`;
  ctx.labelEl.textContent = label;
  ctx.labelEl.classList.remove("danger");
}

function hideIndexProgress(ctx: DocCtx) {
  ctx.progressEl.hidden = true;
  ctx.fillEl.style.width = "0";
}

// Краткая надпись в области прогресса (итог/ошибка), затем авто-скрытие. Таймер
// хранится в контексте и отменяется при новой операции (не гасит чужой прогресс).
function flashIndexLabel(text: string, isError: boolean, ctx: DocCtx) {
  if (ctx.flashTimer !== null) clearTimeout(ctx.flashTimer);
  ctx.progressEl.hidden = false;
  ctx.fillEl.style.width = "0";
  ctx.labelEl.textContent = text;
  ctx.labelEl.classList.toggle("danger", isError);
  ctx.flashTimer = window.setTimeout(() => {
    ctx.labelEl.classList.remove("danger");
    ctx.progressEl.hidden = true;
    ctx.flashTimer = null;
  }, 3500);
}

// «Добавить документ» в контекст (сайдбар или проект): выбор файла → индексация.
async function addDocument(ctx: DocCtx) {
  let path: string | null;
  try {
    const sel = await open({
      multiple: false,
      filters: [{ name: "Документы", extensions: ["pdf", "docx", "txt", "md"] }],
    });
    path = typeof sel === "string" ? sel : null;
  } catch (e) {
    flashIndexLabel(`Не удалось открыть диалог: ${e}`, true, ctx);
    return;
  }
  if (!path) return;

  ctx.addBtn.disabled = true;
  const myOp = ++ctx.opGen; // виджет прогресса принадлежит этой операции (как myGen в send)
  showIndexProgress("Чтение документа…", 0.04, ctx);

  const onProgress = new Channel<IndexProgress>();
  onProgress.onmessage = (p) => {
    if (myOp !== ctx.opGen) return; // операция уже завершена/сменилась — хвост не рисуем
    const frac = p.total ? p.current / p.total : 0;
    if (p.phase === "chunk") showIndexProgress(`Подготовка фрагментов: ${p.total}`, 0.08, ctx);
    else if (p.phase === "embed") showIndexProgress(`Индексация: ${p.current} из ${p.total}`, frac, ctx);
    else if (p.phase === "done") showIndexProgress("Сохранение…", 1, ctx);
  };

  try {
    const res = await invoke<{ status: string; document: DocumentMeta; rebuilt: boolean }>(
      "index_document",
      { path, projectId: ctx.projectId, onProgress },
    );
    ctx.opGen++; // операция завершена: запоздавший progress ЕЁ ЖЕ канала не затрёт итог
    await refreshDocuments(ctx);
    if (ctx.projectId) await refreshCurrentDocsCount(); // могли пополнить базу открытого чата
    if (res.rebuilt) {
      flashIndexLabel("База пересоздана под новую модель поиска — прежние документы добавьте заново", true, ctx);
    } else if (res.status === "exists") {
      flashIndexLabel(`«${res.document.filename}» уже в базе`, false, ctx);
    } else {
      flashIndexLabel(`Добавлен: ${res.document.filename}`, false, ctx);
    }
  } catch (e) {
    ctx.opGen++;
    hideIndexProgress(ctx);
    flashIndexLabel(humanError(e), true, ctx);
  } finally {
    ctx.addBtn.disabled = false;
  }
}

async function deleteDocument(d: DocumentMeta, ctx: DocCtx) {
  if (!(await confirmModal(`Удалить «${d.filename}» из базы документов?`))) return;
  try {
    await invoke("delete_document", { id: d.id });
  } catch (e) {
    flashIndexLabel(`Не удалось удалить: ${humanError(e)}`, true, ctx);
    return;
  }
  await refreshDocuments(ctx);
  await refreshCurrentDocsCount();
}

// Есть ли документы в базе ОТКРЫТОГО чата (его проекта или общей — вне проектов).
// Определяет, запускать ли RAG-поиск перед ответом (docsCount как флаг 0/1).
async function refreshCurrentDocsCount() {
  try {
    const empty = await invoke<boolean>("documents_empty", { projectId: currentProjectId });
    docsCount = empty ? 0 : 1;
  } catch {
    docsCount = 0;
  }
}

// Сырые технические ошибки (reqwest/serde/Ollama) → короткий человеческий текст.
// Детали остаются в console для диагностики; пользователю — понятная суть.
function humanError(e: unknown): string {
  const s = String(e);
  console.error(s);
  if (/connect|Connection|11434|отказано|refused/i.test(s))
    return "Движок Ollama недоступен. Проверьте, запущен ли он (кнопка «Проверка»).";
  if (/tim| timed out|timeout/i.test(s)) return "Превышено время ожидания ответа движка.";
  if (/no such|not found|404/i.test(s))
    return "Модель не установлена. Откройте «Настройки → Модели».";
  if (/memory|OOM|allocat/i.test(s))
    return "Не хватает памяти для модели. Выберите модель полегче или меньший контекст.";
  return s.length > 200 ? s.slice(0, 200) + "…" : s;
}

// ── Проекты: список в сайдбаре, экран проекта (знания + чаты + инструкции) ─────

const ICON_PROJECT =
  '<path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/>';

// Тянет список проектов из бэкенда и рисует в боковой панели.
async function refreshProjects() {
  try {
    projects = await invoke<Project[]>("list_projects");
  } catch (e) {
    console.error("list_projects:", e);
    projects = [];
  }
  renderProjectList();
}

function renderProjectList() {
  projectListEl.innerHTML = "";
  if (projects.length === 0) {
    const empty = document.createElement("div");
    empty.className = "project-empty";
    empty.textContent = "Проектов пока нет";
    projectListEl.appendChild(empty);
    return;
  }
  for (const p of projects) {
    const item = document.createElement("div");
    item.className = "project" + (p.id === viewingProjectId ? " active" : "");
    item.innerHTML = `<svg viewBox="0 0 24 24">${ICON_PROJECT}</svg>`;

    const name = document.createElement("span");
    name.className = "name";
    name.textContent = p.name || "Без названия";
    item.appendChild(name);

    item.addEventListener("click", () => openProjectView(p.id));
    projectListEl.appendChild(item);
  }
}

// Создать проект: спрашиваем имя, сохраняем, открываем его экран.
async function createProject() {
  const name = await promptModal("Название проекта", "Например: Договоры с поставщиками");
  if (name === null) return;
  const trimmed = name.trim() || "Новый проект";
  const now = Date.now();
  const proj: Project = { id: crypto.randomUUID(), name: trimmed, instructions: "", created_at: now, updated_at: now };
  try {
    await invoke("save_project", { project: proj });
  } catch (e) {
    addError(`Не удалось создать проект: ${humanError(e)}`);
    return;
  }
  await refreshProjects();
  openProjectView(proj.id);
}

// Сохранить имя/инструкции проекта (upsert). Тихо — это фоновое сохранение.
async function saveProjectMeta(proj: Project) {
  proj.updated_at = Date.now();
  try {
    await invoke("save_project", { project: proj });
    await refreshProjects();
  } catch (e) {
    projectStatus(`Не удалось сохранить: ${humanError(e)}`, true);
  }
}

function projectStatus(text: string, isError: boolean) {
  projectStatusEl.hidden = false;
  projectStatusEl.textContent = text;
  projectStatusEl.classList.toggle("settings-status--error", isError);
  window.setTimeout(() => (projectStatusEl.hidden = true), 2500);
}

// Открыть полноэкранный экран проекта: инструкции, знания (документы), чаты проекта.
function openProjectView(id: string) {
  const proj = projects.find((p) => p.id === id);
  if (!proj) return;
  if (streaming) stop();
  viewingProjectId = id;
  // Прячем ленту/композер и настройки, показываем экран проекта (как настройки).
  feedEl.hidden = true;
  composerWrapEl.hidden = true;
  settingsView.hidden = true;
  settingsBtn.classList.remove("active");
  projectView.hidden = false;

  projectNameInput.value = proj.name;
  projectInstructionsEl.value = proj.instructions;
  projectStatusEl.hidden = true;

  projectDocCtx.projectId = id;
  refreshDocuments(projectDocCtx);
  renderProjectChats(id);
  renderProjectList(); // подсветить активный
}

function closeProjectView() {
  projectView.hidden = true;
  viewingProjectId = null;
  feedEl.hidden = false;
  composerWrapEl.hidden = false;
  renderProjectList();
}

// Список чатов конкретного проекта (внутри экрана проекта).
function renderProjectChats(projectId: string) {
  projectChatListEl.innerHTML = "";
  const items = convMetas.filter((m) => m.project_id === projectId);
  if (items.length === 0) {
    const empty = document.createElement("div");
    empty.className = "project-empty";
    empty.textContent = "В проекте пока нет чатов. Начните новый — он будет искать по документам проекта.";
    projectChatListEl.appendChild(empty);
    return;
  }
  for (const m of items) {
    const item = document.createElement("div");
    item.className = "conv";
    item.innerHTML = `<svg viewBox="0 0 24 24">${ICON_CHAT}</svg>`;
    const name = document.createElement("span");
    name.className = "name";
    name.textContent = m.title || "Без названия";
    item.appendChild(name);

    const del = document.createElement("button");
    del.className = "conv-del";
    del.textContent = "×";
    del.title = "Удалить чат";
    del.addEventListener("click", (e) => {
      e.stopPropagation();
      deleteConversation(m.id);
    });
    item.appendChild(del);

    item.addEventListener("click", () => openConversation(m.id));
    projectChatListEl.appendChild(item);
  }
}

// Удалить проект вместе с чатами и документами (каскад на бэкенде).
async function deleteProjectFlow() {
  if (!viewingProjectId) return;
  const proj = projects.find((p) => p.id === viewingProjectId);
  const ok = await confirmModal(
    `Удалить проект «${proj?.name ?? ""}» со всеми его чатами и документами? Это необратимо.`,
  );
  if (!ok) return;
  const id = viewingProjectId;
  try {
    await invoke("delete_project", { id });
  } catch (e) {
    addError(`Не удалось удалить проект: ${humanError(e)}`);
    return;
  }
  // Если открытый чат принадлежал этому проекту — уводим в новый быстрый чат.
  if (currentProjectId === id) newDialog(null);
  closeProjectView();
  await refreshProjects();
  await refreshConversationList();
}

// Индикатор в шапке: к какому проекту относится ОТКРЫТЫЙ чат (клик — открыть проект).
function updateChatContextChip() {
  const proj = currentProjectId ? projects.find((p) => p.id === currentProjectId) : null;
  if (proj) {
    chatProjectChip.hidden = false;
    chatProjectChipName.textContent = proj.name;
  } else {
    chatProjectChip.hidden = true;
  }
}

// ── Установка модели эмбеддингов (операционный слой): pull с прогрессом ───────

// Частые статусы Ollama /api/pull → понятный русский.
function ruPullStatus(status: string): string {
  if (status.startsWith("pulling manifest")) return "Получение манифеста";
  if (status.startsWith("pulling")) return "Скачивание";
  if (status.startsWith("verifying")) return "Проверка";
  if (status.startsWith("writing")) return "Запись";
  if (status.startsWith("removing")) return "Очистка";
  if (status === "success") return "Готово";
  return status || "Установка";
}

function gb(bytes: number): string {
  return `${(bytes / 1024 / 1024 / 1024).toFixed(1)} ГБ`;
}

// Адаптер поверхности установки: как рисовать прогресс и итог в её виджетах.
// Единый runPull обслуживает все три поверхности (карточка bge-m3 в сайдбаре,
// каталог моделей в настройках, предложение vision-модели в чате).
interface PullUi {
  progress(text: string, frac: number): void;
  done(): void;
  cancelled(): void;
  error(msg: string): void; // msg уже прогнан через humanError
}

// Заблокировать/разблокировать все точки входа установки на время pull: повторный
// вход и параллельные установки запрещены (бэкенд тоже отклонит — здесь мгновенный
// локальный гейт). Кнопки строк каталога пересоздаёт renderModelList — он сам
// смотрит на pullingTag при отрисовке.
function setPullButtonsEnabled(on: boolean): void {
  installEmbedBtn.disabled = !on;
  installLocalBtn.disabled = !on;
  installFromDiskBtn.disabled = !on;
  modelListEl.querySelectorAll<HTMLButtonElement>("button").forEach((b) => (b.disabled = !on));
}

// Единый поток установки модели: канал прогресса + честный итог по результату
// команды (done/cancelled) — «установлено» больше не показывается после отмены.
async function runPull(tag: string, ui: PullUi): Promise<PullOutcome | "error"> {
  if (pullingTag) {
    ui.error(`Уже идёт установка «${pullingTag}» — дождитесь завершения или отмените её.`);
    return "error";
  }
  pullingTag = tag;
  setPullButtonsEnabled(false);
  // Канал и результат команды — разные пути доставки IPC: после итога гасим
  // запоздавшие progress-сообщения, чтобы они не затирали финальную надпись.
  let settled = false;
  const onEvent = new Channel<PullEvent>();
  onEvent.onmessage = (e) => {
    if (settled) return;
    const frac = e.total > 0 ? e.completed / e.total : 0;
    const tail =
      e.total > 0 ? ` ${Math.round(frac * 100)}% (${gb(e.completed)} из ${gb(e.total)})` : "";
    ui.progress(`${ruPullStatus(e.status)}${tail}`, frac);
  };
  try {
    const outcome = await invoke<PullOutcome>("pull_model", { name: tag, onEvent });
    settled = true;
    if (outcome === "cancelled") ui.cancelled();
    else ui.done();
    return outcome;
  } catch (e) {
    settled = true;
    ui.error(humanError(e));
    return "error";
  } finally {
    pullingTag = null;
    setPullButtonsEnabled(true);
  }
}

// Отмена активной установки — кнопки всех поверхностей ведут сюда. Итог придёт
// результатом pull_model, и поверхность отреагирует своей веткой cancelled.
function cancelActivePull() {
  pullCancelBtn.disabled = true;
  modelPullCancelBtn.disabled = true;
  invoke("cancel_pull").catch(() => {});
}

// «Установить bge-m3»: единый runPull с прогрессом в панели вкладки «Документы».
// Возвращает итог — вызывающий решает, что обновлять после успеха.
async function installEmbeddingModel(): Promise<PullOutcome | "error"> {
  const ctx = sidebarDocCtx;
  const myOp = ++ctx.opGen; // виджет прогресса теперь принадлежит этой операции
  installEmbedBtn.hidden = true;
  docStatusEl.hidden = true; // на месте карточки — прогресс
  pullCancelBtn.hidden = false;
  pullCancelBtn.disabled = false;
  showIndexProgress("Подготовка установки…", 0.02, ctx);

  const outcome = await runPull("bge-m3", {
    progress: (t, f) => {
      if (myOp === ctx.opGen) showIndexProgress(t, f, ctx);
    },
    done: () => {
      if (myOp === ctx.opGen) flashIndexLabel("Модель bge-m3 установлена", false, ctx);
    },
    cancelled: () => {
      if (myOp === ctx.opGen)
        flashIndexLabel(
          "Установка отменена — можно докачать позже (Ollama продолжит с места)",
          false,
          ctx,
        );
    },
    error: (m) => {
      if (myOp === ctx.opGen) flashIndexLabel(m, true, ctx);
    },
  });

  pullCancelBtn.hidden = true;
  if (outcome === "done") {
    // модель появилась — обновляем статус базы и список моделей без перезапуска
    await refreshDocuments(ctx);
    await loadModels();
  } else if (!embeddingReady) {
    // не установилась — вернуть карточку с кнопкой
    docStatusEl.hidden = false;
    installEmbedBtn.hidden = false;
    installEmbedBtn.disabled = false;
  }
  return outcome;
}

// ── Левая панель: изменение ширины и сворачивание ────────────────────────────

const SIDEBAR_MIN = 200; // нижняя граница ширины
const SIDEBAR_MAX = 420; // верхняя граница ширины

// Установить ширину панели (в пределах [MIN, MAX]); опц. сохранить в настройки.
function setSidebarWidth(px: number, persist: boolean) {
  const w = Math.max(SIDEBAR_MIN, Math.min(SIDEBAR_MAX, Math.round(px)));
  document.documentElement.style.setProperty("--side-w", `${w}px`);
  if (persist) {
    invoke("set_setting", { key: "sidebar_width", value: String(w) }).catch(() => {});
  }
}

// Перетаскивание ручки у правого края панели. Ширина = позиция курсора по X
// (панель прижата к левому краю окна). Сохраняем на отпускании.
function startSidebarResize(e: PointerEvent) {
  e.preventDefault();
  document.body.classList.add("resizing");
  const move = (ev: PointerEvent) => setSidebarWidth(ev.clientX, false);
  const up = (ev: PointerEvent) => {
    document.removeEventListener("pointermove", move);
    document.removeEventListener("pointerup", up);
    document.body.classList.remove("resizing");
    setSidebarWidth(ev.clientX, true);
  };
  document.addEventListener("pointermove", move);
  document.addEventListener("pointerup", up);
}

// Свернуть/показать панель (состояние сохраняется).
function toggleSidebar() {
  const collapsed = appEl.classList.toggle("sidebar-collapsed");
  invoke("set_setting", { key: "sidebar_collapsed", value: String(collapsed) }).catch(() => {});
}

// Восстановить ширину и состояние панели из настроек при старте.
async function initSidebar() {
  try {
    const w = await invoke<string | null>("get_setting", { key: "sidebar_width" });
    if (w) {
      const n = parseInt(w, 10);
      if (!Number.isNaN(n)) setSidebarWidth(n, false);
    }
    const collapsed = await invoke<string | null>("get_setting", { key: "sidebar_collapsed" });
    if (collapsed === "true") appEl.classList.add("sidebar-collapsed");
  } catch {
    /* настройки недоступны — ширина по умолчанию */
  }
}

// ── Страница настроек (на месте ленты диалогов) ──────────────────────────────

// Открыть настройки: лента и поле ввода скрываются, страница занимает их место.
function openSettings() {
  if (!projectView.hidden) closeProjectView(); // настройки и экран проекта взаимоисключаемы
  feedEl.hidden = true;
  composerWrapEl.hidden = true;
  settingsView.hidden = false;
  settingsBtn.classList.add("active");
  refreshEnginePaths(); // подтянуть актуальные пути при открытии
  loadModelStates(); // локальные состояния моделей набора (статус — в бейджах строк)
  resetCheckButton(); // кнопка проверки — всегда в исходном виде на открытии
  refreshOutboundLog(); // актуальный журнал обращений в интернет
  runDiagnostics(); // самопроверка при каждом открытии (локально, дёшево)
}

// Вернуться назад: страница скрывается, лента и поле ввода возвращаются.
function closeSettings() {
  settingsView.hidden = true;
  feedEl.hidden = false;
  composerWrapEl.hidden = false;
  settingsBtn.classList.remove("active");
  resetCheckButton(); // уход со страницы — кнопка к исходному (итог в сессии сохранён)
  modelsStatusEl.hidden = true;
  if (!streaming && selectedModel) inputEl.focus();
}

// ── Офлайн-поставка: override-пути движка/моделей (без интернета) ─────────────

// Текущие override-пути на странице настроек (из settings.json).
async function refreshEnginePaths() {
  try {
    const models = await invoke<string | null>("get_setting", { key: "ollama_models_dir" });
    const engine = await invoke<string | null>("get_setting", { key: "ollama_path" });
    epModelsEl.textContent = models || "по умолчанию";
    epEngineEl.textContent = engine || "по умолчанию";
  } catch {
    /* настройки недоступны — оставляем как есть */
  }
}

// Статус-сообщение на странице настроек.
function settingsStatus(text: string, isError: boolean) {
  settingsStatusEl.hidden = false;
  settingsStatusEl.textContent = text;
  settingsStatusEl.classList.toggle("settings-status--error", isError);
}

// Выбор каталога моделей Ollama через системный диалог.
async function pickModelsDir(): Promise<string | null> {
  try {
    const sel = await open({ directory: true, multiple: false, title: "Каталог моделей Ollama" });
    return typeof sel === "string" ? sel : null;
  } catch {
    return null;
  }
}

// Ядро применения локального каталога моделей: запись override (с валидацией) →
// перезапуск нашего движка с новым OLLAMA_MODELS (или честно про внешний). Без сети.
// report — контекстная обратная связь (карточка «Документы» либо страница настроек).
async function applyModelsDir(dir: string, report: (t: string, err: boolean) => void) {
  try {
    await invoke("set_models_dir", { path: dir }); // валидация manifests/blobs + запись
    const res = await invoke<{ status: string; message: string }>("reload_engine");
    await refreshDocuments(sidebarDocCtx);
    await loadModels();
    if (res.status === "external") report(res.message, false);
    else if (embeddingReady) report("Локальный каталог моделей применён", false);
    else report("Каталог применён, но bge-m3 в нём не найдена", true);
  } catch (e) {
    report(humanError(e), true); // напр. «не похоже на каталог моделей Ollama»
  } finally {
    refreshEnginePaths();
  }
}

// «Указать локально» из карточки в «Документах» — обратная связь в прогресс-панель.
async function installFromLocalDir() {
  const dir = await pickModelsDir();
  if (!dir) return;
  installEmbedBtn.disabled = true;
  installLocalBtn.disabled = true;
  showIndexProgress("Применение локального каталога…", 0.4, sidebarDocCtx);
  await applyModelsDir(dir, (t, err) => flashIndexLabel(t, err, sidebarDocCtx));
  installEmbedBtn.disabled = false;
  installLocalBtn.disabled = false;
}

// «Указать…» каталог моделей со страницы настроек — обратная связь там же.
async function settingsPickModels() {
  const dir = await pickModelsDir();
  if (!dir) return;
  settingsStatus("Применение локального каталога…", false);
  await applyModelsDir(dir, settingsStatus);
}

// «Указать…» исполняемый файл движка (air-gapped, когда Ollama нет в PATH).
async function setEnginePathDialog() {
  let file: string | null;
  try {
    const sel = await open({ multiple: false, title: "Исполняемый файл Ollama" });
    file = typeof sel === "string" ? sel : null;
  } catch (e) {
    settingsStatus(`Не удалось открыть диалог: ${e}`, true);
    return;
  }
  if (!file) return;
  try {
    await invoke("set_engine_path", { path: file }); // валидация исполняемого файла
    settingsStatus("Путь к движку сохранён (применится при следующем запуске движка).", false);
  } catch (e) {
    settingsStatus(String(e), true);
  }
  refreshEnginePaths();
}

// «Сбросить»: вернуться к авто-разрешению (ресурс → PATH) и применить к движку.
async function resetEnginePaths() {
  if (!(await confirmModal("Сбросить override-пути движка и каталога моделей?", "Сбросить"))) return;
  try {
    await invoke("clear_engine_overrides");
    await invoke("reload_engine");
    await refreshDocuments(sidebarDocCtx);
    await loadModels();
    settingsStatus("Пути сброшены — авто-разрешение (ресурс → PATH).", false);
  } catch (e) {
    settingsStatus(String(e), true);
  }
  refreshEnginePaths();
}

// ── Управление моделями (M4): состояние, установка/обновление, источник ───────

let modelsStatusTimer: number | undefined;
function modelsStatus(text: string, isError: boolean) {
  modelsStatusEl.hidden = false;
  modelsStatusEl.textContent = text;
  modelsStatusEl.classList.toggle("settings-status--error", isError);
  if (modelsStatusTimer) clearTimeout(modelsStatusTimer);
  // успех не висит постоянно — прячем через несколько секунд; ошибки остаются
  if (!isError) {
    modelsStatusTimer = window.setTimeout(() => {
      modelsStatusEl.hidden = true;
    }, 3500);
  }
}

// Локальные состояния моделей набора (без сети) → отрисовка списка.
async function loadModelStates() {
  try {
    const res = await invoke<{ models: ModelState[]; others: string[] }>("model_states");
    modelStates = res.models;
  } catch (e) {
    modelStates = [];
    modelsStatus(`Не удалось получить список моделей: ${humanError(e)}`, true);
  }
  renderModelList();
}

// Бейдж состояния модели с учётом онлайн-проверки обновлений.
function modelBadge(m: ModelState): { text: string; cls: string } {
  if (!m.installed) return { text: "не установлена", cls: "model-badge--missing" };
  const upd = updateByTag.get(m.tag);
  if (upd === "update") return { text: "есть обновление", cls: "model-badge--update" };
  if (upd === "current") return { text: "актуальна", cls: "model-badge--ok" };
  return { text: "установлена", cls: "model-badge--ok" }; // обновление ещё не проверяли
}

// Иконка по роли модели (stroke-стиль, как в топбаре).
function roleIcon(role: string): string {
  const paths: Record<string, string> = {
    text: '<path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z"/>',
    embed: '<circle cx="11" cy="11" r="7"/><path d="m21 21-4.3-4.3"/>',
    vision: '<path d="M2 12s3-7 10-7 10 7 10 7-3 7-10 7-10-7-10-7z"/><circle cx="12" cy="12" r="3"/>',
    code: '<path d="m16 18 6-6-6-6M8 6l-6 6 6 6"/>',
  };
  return `<svg viewBox="0 0 24 24">${paths[role] ?? paths.text}</svg>`;
}

const ICON_DOWNLOAD = '<svg viewBox="0 0 24 24"><path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4M7 10l5 5 5-5M12 15V3"/></svg>';
const ICON_REFRESH_CW = '<svg viewBox="0 0 24 24"><path d="M3 12a9 9 0 0 1 15-6.7L21 8M21 3v5h-5M21 12a9 9 0 0 1-15 6.7L3 16M3 21v-5h5"/></svg>';
const ICON_CHECK = '<svg viewBox="0 0 24 24"><path d="M20 6 9 17l-5-5"/></svg>';
const ICON_ALERT = '<svg viewBox="0 0 24 24"><path d="M10.3 3.9 1.8 18a2 2 0 0 0 1.7 3h16.9a2 2 0 0 0 1.7-3L13.7 3.9a2 2 0 0 0-3.4 0z"/><path d="M12 9v4M12 17h.01"/></svg>';

// Итог последней проверки обновлений (для подсчётов). Постоянный статус каждой
// модели живёт в бейджах строк (updateByTag), а не в кнопке.
let lastCheck: { current: number; updates: number; errors: number } | null = null;
let checkBtnTimer: number | undefined;

// Показать итог проверки В КНОПКЕ кратко (цветом), затем вернуть её к исходному виду.
// Кнопка не «залипает» — постоянное состояние видно по бейджам у моделей.
function showCheckResult() {
  checkUpdatesBtn.disabled = false;
  checkUpdatesBtn.classList.remove("checking", "check-ok", "check-update", "check-err");
  if (checkBtnTimer) clearTimeout(checkBtnTimer);
  if (!lastCheck) {
    resetCheckButton();
    return;
  }
  if (lastCheck.errors > 0) {
    checkUpdatesBtn.classList.add("check-err");
    checkUpdatesBtn.innerHTML = `${ICON_REFRESH_CW}Нет сети`;
  } else if (lastCheck.updates > 0) {
    checkUpdatesBtn.classList.add("check-update");
    checkUpdatesBtn.innerHTML = `${ICON_ALERT}Есть обновления (${lastCheck.updates})`;
  } else {
    checkUpdatesBtn.classList.add("check-ok");
    checkUpdatesBtn.innerHTML = `${ICON_CHECK}Актуально`;
  }
  // показать кратко и вернуть к исходному виду (не гореть постоянно)
  checkBtnTimer = window.setTimeout(resetCheckButton, 3500);
}

// Вернуть кнопку к исходному виду «Проверить обновления».
function resetCheckButton() {
  if (checkBtnTimer) {
    clearTimeout(checkBtnTimer);
    checkBtnTimer = undefined;
  }
  checkUpdatesBtn.disabled = false;
  checkUpdatesBtn.classList.remove("check-ok", "check-update", "check-err", "checking");
  checkUpdatesBtn.innerHTML = `${ICON_REFRESH_CW}Проверить обновления`;
  checkUpdatesBtn.title = "Сверить версии с реестром Ollama (нужен интернет)";
}

// Пересчитать итог из текущих статусов и кратко показать (после установки/обновления).
function recomputeLastCheck() {
  if (!lastCheck) return; // проверки не было — кнопку не трогаем
  let current = 0;
  let updates = 0;
  let errors = 0;
  for (const m of modelStates) {
    const s = updateByTag.get(m.tag);
    if (s === "current") current++;
    else if (s === "update") updates++;
    else if (s === "error") errors++;
  }
  lastCheck = { current, updates, errors };
  showCheckResult();
}

function renderModelList() {
  modelListEl.innerHTML = "";
  for (const m of modelStates) {
    const row = document.createElement("div");
    row.className = "model-row";

    const icon = document.createElement("span");
    icon.className = "model-row__icon";
    icon.innerHTML = roleIcon(m.role);
    row.appendChild(icon);

    const info = document.createElement("div");
    info.className = "model-row__info";
    const title = document.createElement("div");
    title.className = "model-row__title";
    title.textContent = m.title;
    if (m.required) {
      const req = document.createElement("span");
      req.className = "model-row__req";
      req.textContent = "обязательная";
      title.appendChild(req);
    }
    const tag = document.createElement("div");
    tag.className = "model-row__tag";
    tag.textContent = m.tag + (m.size ? ` · ${gb(m.size)}` : ""); // единый форматтер байтов
    info.append(title, tag);

    const badge = document.createElement("span");
    const b = modelBadge(m);
    badge.className = `model-badge ${b.cls}`;
    badge.textContent = b.text;

    row.append(info, badge);

    // действие: Установить (нет локально) / Обновить (есть обновление)
    const upd = updateByTag.get(m.tag);
    const needsAction = !m.installed || upd === "update";
    if (needsAction) {
      const btn = document.createElement("button");
      btn.className = "ep-btn ep-btn--icon";
      btn.innerHTML = m.installed
        ? `${ICON_REFRESH_CW}Обновить`
        : `${ICON_DOWNLOAD}Установить`;
      btn.disabled = pullingTag !== null; // во время активной установки — заблокировано
      btn.addEventListener("click", () => pullModelTag(m.tag, m.installed));
      row.appendChild(btn);
    }
    modelListEl.appendChild(row);
  }
}

// Проверка обновлений (онлайн, по кнопке): сравнение digest с реестром.
// Итог пишется ПРЯМО В КНОПКУ (Актуально/Есть обновления/Нет сети) и держится
// на странице; сохраняется в сессии и восстанавливается при возврате.
async function checkModelUpdates() {
  checkUpdatesBtn.disabled = true;
  checkUpdatesBtn.classList.remove("check-ok", "check-update", "check-err");
  checkUpdatesBtn.classList.add("checking"); // запускаем вращение стрелок
  checkUpdatesBtn.innerHTML = `${ICON_REFRESH_CW}Проверка…`;
  try {
    const res = await invoke<{ tag: string; status: string }[]>("check_model_updates");
    updateByTag.clear();
    let current = 0;
    let updates = 0;
    let errors = 0;
    for (const r of res) {
      updateByTag.set(r.tag, r.status);
      if (r.status === "current") current++;
      else if (r.status === "update") updates++;
      else if (r.status === "error") errors++;
    }
    renderModelList();
    lastCheck = { current, updates, errors };
  } catch {
    lastCheck = { current: 0, updates: 0, errors: 1 };
  } finally {
    showCheckResult();
  }
}

// Установка/обновление модели онлайн — единый runPull с прогрессом в строке раздела.
// «Установлено» и статус «актуальна» — ТОЛЬКО при честном done (не после отмены).
async function pullModelTag(tag: string, isUpdate: boolean) {
  const verb = isUpdate ? "Обновление" : "Установка";
  modelProgressEl.hidden = false;
  modelPullCancelBtn.hidden = false;
  modelPullCancelBtn.disabled = false;
  modelProgressFill.style.width = "4%";
  modelProgressLabel.textContent = `${verb} ${tag}…`;

  const outcome = await runPull(tag, {
    progress: (t, f) => {
      modelProgressFill.style.width = `${Math.max(4, Math.round(f * 100))}%`;
      modelProgressLabel.textContent = `${verb} ${tag}: ${t}`;
    },
    done: () => modelsStatus(`${tag}: ${isUpdate ? "обновлено" : "установлено"}.`, false),
    cancelled: () =>
      modelsStatus(
        `${tag}: ${isUpdate ? "обновление отменено" : "установка отменена"} — докачка продолжится при повторе.`,
        false,
      ),
    error: (m) => modelsStatus(`Не удалось ${isUpdate ? "обновить" : "установить"} ${tag}: ${m}`, true),
  });

  modelProgressEl.hidden = true;
  modelPullCancelBtn.hidden = true;
  if (outcome === "done") {
    updateByTag.set(tag, "current"); // только что подтянули — актуальна
    await loadModelStates();
    await loadModels(); // обновить выпадающий список моделей
    await refreshDocuments(sidebarDocCtx); // вдруг поставили bge-m3 — RAG ожил
    recomputeLastCheck(); // итог в кнопке — с учётом установленного/обновлённого
  }
}

// Источник «с диска»: указать локальный каталог моделей (переиспуем офлайн-поставку).
async function installFromDiskForModels() {
  const dir = await pickModelsDir();
  if (!dir) return;
  modelsStatus("Применение локального каталога…", false);
  await applyModelsDir(dir, modelsStatus);
  await loadModelStates();
}

// ── Проверка системы (диагностика): раздел настроек ──────────────────────────

interface DiagCheck {
  id: string;
  title: string;
  status: string; // "ok" | "warn" | "fail"
  detail: string;
}

// Иконки проверок — в стиле roleIcon (контурные, 24×24).
function diagIcon(id: string): string {
  const paths: Record<string, string> = {
    engine: '<circle cx="12" cy="12" r="3"/><path d="M12 2v3M12 19v3M2 12h3M19 12h3M4.9 4.9l2.2 2.2M16.9 16.9l2.2 2.2M4.9 19.1l2.2-2.2M16.9 7.1l2.2-2.2"/>',
    models: '<path d="M12 2 2 7l10 5 10-5-10-5z"/><path d="M2 17l10 5 10-5M2 12l10 5 10-5"/>',
    documents: '<path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z"/><path d="M14 2v6h6"/>',
    disk: '<circle cx="12" cy="12" r="10"/><circle cx="12" cy="12" r="3"/>',
    hardware: '<rect x="4" y="4" width="16" height="16" rx="2"/><rect x="9" y="9" width="6" height="6"/><path d="M9 1v3M15 1v3M9 20v3M15 20v3M1 9h3M1 15h3M20 9h3M20 15h3"/>',
    storage: '<path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/>',
  };
  return `<svg viewBox="0 0 24 24">${paths[id] ?? paths.engine}</svg>`;
}

// Бейдж по статусу проверки (переиспользуем стили бейджей моделей).
function diagBadge(status: string): { cls: string; text: string } {
  if (status === "ok") return { cls: "model-badge--ok", text: "ОК" };
  if (status === "warn") return { cls: "model-badge--update", text: "Внимание" };
  return { cls: "model-badge--err", text: "Проблема" };
}

// Запустить самопроверку и нарисовать результат (строки — как список моделей).
async function runDiagnostics() {
  diagRunBtn.disabled = true;
  diagRunBtn.classList.add("checking");
  diagRunBtn.innerHTML = `${ICON_REFRESH_CW}Проверка…`;
  try {
    const checks = await invoke<DiagCheck[]>("run_diagnostics");
    diagListEl.innerHTML = "";
    for (const c of checks) {
      const row = document.createElement("div");
      row.className = "model-row";

      const icon = document.createElement("span");
      icon.className = "model-row__icon";
      icon.innerHTML = diagIcon(c.id);

      const info = document.createElement("div");
      info.className = "model-row__info";
      const title = document.createElement("div");
      title.className = "model-row__title";
      title.textContent = c.title;
      const detail = document.createElement("div");
      detail.className = "model-row__tag";
      detail.textContent = c.detail;
      info.append(title, detail);

      const badge = document.createElement("span");
      const b = diagBadge(c.status);
      badge.className = `model-badge ${b.cls}`;
      badge.textContent = b.text;

      row.append(icon, info, badge);
      diagListEl.appendChild(row);
    }
  } catch (e) {
    diagListEl.innerHTML = "";
    const err = document.createElement("div");
    err.className = "model-row__tag";
    err.textContent = `Не удалось выполнить проверку: ${e}`;
    diagListEl.appendChild(err);
  } finally {
    diagRunBtn.disabled = false;
    diagRunBtn.classList.remove("checking");
    diagRunBtn.innerHTML = `${ICON_REFRESH_CW}Проверить`;
  }
}

// ── Обновления приложения: онлайн-проверка (по кнопке) и установка с диска ────

// Статус-сообщение карточки обновлений.
function appUpdateStatus(text: string, isError: boolean) {
  appUpdateStatusEl.hidden = false;
  appUpdateStatusEl.textContent = text;
  appUpdateStatusEl.classList.toggle("settings-status--error", isError);
}

// Проверить наличие новой версии на сервере выпусков (GitHub Releases).
// Единственное сетевое обращение офлайн-продукта — и только по явному нажатию.
async function checkAppUpdate() {
  appUpdateCheckBtn.disabled = true;
  appUpdateCheckBtn.classList.add("checking");
  appUpdateCheckBtn.innerHTML = `${ICON_REFRESH_CW}Проверка…`;
  appUpdateStatusEl.hidden = true;
  appUpdateInfoEl.innerHTML = "";
  try {
    const update = await check();
    if (update) renderAppUpdateRow(update);
    else appUpdateStatus("У вас последняя версия.", false);
  } catch (e) {
    appUpdateStatus(`Не удалось проверить обновления (нужен доступ в интернет): ${e}`, true);
  } finally {
    appUpdateCheckBtn.disabled = false;
    appUpdateCheckBtn.classList.remove("checking");
    appUpdateCheckBtn.innerHTML = `${ICON_REFRESH_CW}Проверить обновления`;
  }
}

// Строка «доступна версия X» с кнопкой установки — в стиле списка моделей.
function renderAppUpdateRow(update: Update) {
  appUpdateInfoEl.innerHTML = "";
  const row = document.createElement("div");
  row.className = "model-row";

  const icon = document.createElement("span");
  icon.className = "model-row__icon";
  icon.innerHTML =
    '<svg viewBox="0 0 24 24"><path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><path d="M7 10l5 5 5-5"/><path d="M12 15V3"/></svg>';

  const info = document.createElement("div");
  info.className = "model-row__info";
  const title = document.createElement("div");
  title.className = "model-row__title";
  title.textContent = `Доступна версия ${update.version}`;
  const detail = document.createElement("div");
  detail.className = "model-row__tag";
  detail.textContent = update.body?.trim() || `Установлена ${update.currentVersion}`;
  info.append(title, detail);

  const btn = document.createElement("button");
  btn.type = "button";
  btn.className = "ep-btn";
  btn.textContent = "Установить";
  btn.addEventListener("click", () => installAppUpdate(update, btn));

  row.append(icon, info, btn);
  appUpdateInfoEl.appendChild(row);
}

// Скачать и установить обновление; подпись проверяется плагином (публичный ключ
// зашит в приложение). После установки — перезапуск (на Windows установщик сам
// закрывает и перезапускает приложение).
async function installAppUpdate(update: Update, btn: HTMLButtonElement) {
  btn.disabled = true;
  appUpdateProgressEl.hidden = false;
  appUpdateProgressFill.style.width = "2%";
  appUpdateProgressLabel.textContent = `Загрузка ${update.version}…`;
  let total = 0;
  let got = 0;
  try {
    await update.downloadAndInstall((e) => {
      if (e.event === "Started") {
        total = e.data.contentLength ?? 0;
      } else if (e.event === "Progress") {
        got += e.data.chunkLength;
        if (total > 0) {
          const pct = Math.round((got / total) * 100);
          appUpdateProgressFill.style.width = `${pct}%`;
          appUpdateProgressLabel.textContent = `Загрузка ${update.version}: ${pct}%`;
        }
      } else if (e.event === "Finished") {
        appUpdateProgressFill.style.width = "100%";
        appUpdateProgressLabel.textContent = "Установка…";
      }
    });
    appUpdateProgressEl.hidden = true;
    appUpdateStatus("Обновление установлено — приложение перезапустится.", false);
    await relaunch();
  } catch (e) {
    appUpdateProgressEl.hidden = true;
    appUpdateStatus(`Не удалось установить обновление: ${e}`, true);
    btn.disabled = false;
  }
}

// Обновление с диска (без интернета): выбрать файл установщика новой версии —
// бэкенд запустит его штатным для ОС способом и закроет приложение.
async function installAppUpdateFromDisk() {
  const sel = await open({
    multiple: false,
    title: "Файл установщика новой версии",
    filters: [
      { name: "Установщик", extensions: ["msi", "exe", "deb", "rpm", "AppImage", "dmg"] },
    ],
  }).catch(() => null);
  if (typeof sel !== "string") return;
  try {
    await invoke("install_update_from_disk", { path: sel });
    appUpdateStatus("Установщик запущен — приложение сейчас закроется.", false);
  } catch (e) {
    appUpdateStatus(`${e}`, true);
  }
}

// ── RAG: поиск фрагментов и сборка контекстного сообщения ────────────────────

// Пакует найденные фрагменты в одно system-сообщение в рамках бюджета символов.
// Возвращает сообщение контекста и список источников (для показа под ответом).
function buildContext(retrieved: RetrievedChunk[]): {
  contextMsg: Message;
  sources: SourceRef[];
} {
  const parts: string[] = [];
  const sources: SourceRef[] = [];
  let used = 0;
  for (const r of retrieved) {
    const block = `[Документ «${r.filename}», фрагмент ${r.chunk_index + 1}]\n${r.text}`;
    if (parts.length && used + block.length > CONTEXT_CHAR_BUDGET) break; // бюджет
    parts.push(block);
    sources.push({ filename: r.filename, chunk_index: r.chunk_index });
    used += block.length;
  }
  const content =
    "Ниже — фрагменты из документов пользователя, которые могут относиться к его вопросу. " +
    "Если вопрос касается этих документов — отвечай, опираясь на фрагменты, и если нужного " +
    "ответа в них нет, честно скажи, что в документах это не найдено, и ничего не придумывай. " +
    "Если же вопрос не связан с этими фрагментами — просто ответь на него как обычно.\n\n" +
    parts.join("\n\n");
  return { contextMsg: { role: "system" as Role, content }, sources };
}

// Рисует строку источников под ответом — но КАЖДЫЙ документ упоминаем один раз за
// диалог. Если все источники этого ответа уже показывались ранее — заметку не рисуем
// (не повторяем под каждым сообщением). Новый документ, попавший в дело, покажем один раз.
function renderSources(turn: HTMLElement, sources: SourceRef[]) {
  if (!sources.length) return;
  const byDoc = new Map<string, number[]>();
  for (const s of sources) {
    if (shownSourceFiles.has(s.filename)) continue; // этот документ уже упоминали
    const arr = byDoc.get(s.filename) ?? [];
    arr.push(s.chunk_index + 1);
    byDoc.set(s.filename, arr);
  }
  if (byDoc.size === 0) return; // все источники уже показаны ранее — не повторяемся
  for (const name of byDoc.keys()) shownSourceFiles.add(name);
  const items = [...byDoc.entries()].map(
    ([name, frags]) => `${name} (фрагм. ${frags.join(", ")})`,
  );
  const row = document.createElement("div");
  row.className = "sources";
  const label = document.createElement("span");
  label.className = "sources__label";
  label.textContent = "Источники: ";
  row.appendChild(label);
  row.appendChild(document.createTextNode(items.join("; ")));
  turn.appendChild(row);
}

// Рисует источники из веб-поиска (онлайн-режим) под ответом — кликабельные ссылки,
// чтобы пользователь видел, откуда взята информация из интернета (152-ФЗ: прозрачность).
function renderWebSources(turn: HTMLElement, items: WebSource[]) {
  if (!items.length) return;
  const row = document.createElement("div");
  row.className = "sources sources--web";
  const label = document.createElement("span");
  label.className = "sources__label";
  label.textContent = "Источники из интернета: ";
  row.appendChild(label);
  items.forEach((s, i) => {
    if (i) row.appendChild(document.createTextNode("; "));
    // URL приходит из ответа стороннего поискового провайдера — не доверяем ему.
    // Разрешаем только http/https; схемы вроде javascript: в привилегированном
    // webview — вектор для запуска Tauri-команд, поэтому такие ссылки не кликабельны.
    let safeUrl: string | null = null;
    try {
      const u = new URL(s.url);
      if (u.protocol === "http:" || u.protocol === "https:") safeUrl = u.href;
    } catch {
      /* невалидный URL — оставим как обычный текст */
    }
    if (safeUrl) {
      const a = document.createElement("a");
      a.className = "source-link";
      a.href = safeUrl;
      a.target = "_blank";
      a.rel = "noopener noreferrer";
      a.textContent = s.title || safeUrl;
      a.title = safeUrl;
      row.appendChild(a);
    } else {
      const span = document.createElement("span");
      span.className = "source-link";
      span.textContent = s.title || s.url;
      row.appendChild(span);
    }
  });
  turn.appendChild(row);
}

// ── Диалоги: сохранение/загрузка/переключение/удаление ──────────────────────

// Заголовок диалога — из первого вопроса пользователя (обрезанный).
function titleFromHistory(): string {
  const first = history.find((m) => m.role === "user");
  if (!first) return "Новый диалог";
  const t = first.content.trim().replace(/\s+/g, " ");
  return t.length > 40 ? t.slice(0, 40) + "…" : t;
}

// Сохраняет текущий диалог на диск и обновляет список. Пустой не сохраняем.
async function persist() {
  if (!history.length || !currentId) return;
  const conv: Conversation = {
    id: currentId,
    title: titleFromHistory(),
    updated_at: Date.now(),
    ...(currentProjectId ? { project_id: currentProjectId } : {}),
    messages: history,
  };
  try {
    await invoke("save_conversation", { conversation: conv });
    await refreshConversationList();
  } catch (e) {
    console.error("save_conversation:", e);
  }
}

// Перерисовывает ленту по текущему массиву history.
function renderHistory() {
  messagesEl.innerHTML = "";
  shownSourceFiles.clear(); // заново считаем «первое упоминание» источников в этом диалоге
  autoScroll = true; // открыли диалог — показываем низ (последние сообщения)
  for (const m of history)
    addBubble(m.role, m.content, m.doc, m.sources, m.images, m.webSources);
  refreshEmptyState(); // пустой диалог → приветствие; иначе скрыто
}

// Перечитывает список диалогов из файлов и перерисовывает боковую панель.
async function refreshConversationList() {
  try {
    convMetas = await invoke<ConversationMeta[]>("list_conversations");
  } catch {
    return;
  }
  renderConvList();
  if (!projectView.hidden && viewingProjectId) renderProjectChats(viewingProjectId);
}

// Группа по дате последнего изменения.
function dateGroup(ts: number): string {
  const today = new Date();
  today.setHours(0, 0, 0, 0);
  const t0 = today.getTime();
  if (ts >= t0) return "Сегодня";
  if (ts >= t0 - 86_400_000) return "Вчера";
  return "Ранее";
}

const ICON_CHAT =
  '<path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z"/>';

// Рисует список БЫСТРЫХ чатов (вне проектов) с учётом поиска и групп по датам.
// Чаты проектов показываются на экране проекта, не в общем списке.
function renderConvList() {
  convListEl.innerHTML = "";
  const f = convFilter.trim().toLowerCase();
  const items = convMetas
    .filter((m) => !m.project_id) // только вне проектов
    .filter((m) => !f || (m.title || "").toLowerCase().includes(f));

  let lastGroup = "";
  for (const m of items) {
    const group = dateGroup(m.updated_at);
    if (group !== lastGroup) {
      const lbl = document.createElement("div");
      lbl.className = "conv-label";
      lbl.textContent = group;
      convListEl.appendChild(lbl);
      lastGroup = group;
    }

    const item = document.createElement("div");
    item.className = "conv" + (m.id === currentId ? " active" : "");
    item.innerHTML = `<svg viewBox="0 0 24 24">${ICON_CHAT}</svg>`;

    const name = document.createElement("span");
    name.className = "name";
    name.textContent = m.title || "Без названия";
    item.appendChild(name);

    const del = document.createElement("button");
    del.className = "conv-del";
    del.textContent = "×";
    del.title = "Удалить диалог";
    del.addEventListener("click", (e) => {
      e.stopPropagation();
      deleteConversation(m.id);
    });
    item.appendChild(del);

    item.addEventListener("click", () => openConversation(m.id));
    convListEl.appendChild(item);
  }
}

// Открывает диалог из файла в ленту. Восстанавливает и проект чата (для RAG/инструкций).
async function openConversation(id: string) {
  if (streaming) stop();
  if (!settingsView.hidden) closeSettings(); // вышли из настроек — показываем ленту
  if (!projectView.hidden) closeProjectView(); // и с экрана проекта — в ленту
  let conv: Conversation;
  try {
    conv = await invoke<Conversation>("load_conversation", { id });
  } catch (e) {
    addError(`Не удалось открыть диалог: ${e}`);
    return;
  }
  currentId = conv.id;
  currentProjectId = conv.project_id ?? null;
  history.length = 0;
  history.push(...conv.messages);
  renderHistory();
  updateChatContextChip();
  refreshCurrentDocsCount(); // база знаний могла быть у проекта этого чата
  refreshConversationList();
  inputEl.focus();
}

// «Новый чат»: пустой чат в указанном проекте (projectId=null — быстрый чат вне
// проектов). Старый уже сохранён — ничего не теряется.
function newDialog(projectId: string | null = null) {
  if (streaming) stop();
  if (!settingsView.hidden) closeSettings(); // вышли из настроек — показываем ленту
  if (!projectView.hidden) closeProjectView();
  currentId = crypto.randomUUID();
  currentProjectId = projectId;
  history.length = 0;
  shownSourceFiles.clear(); // новый диалог — источники снова показываем с первого раза
  messagesEl.innerHTML = "";
  refreshEmptyState(); // пустой диалог → показываем приветствие
  updateChatContextChip();
  refreshCurrentDocsCount(); // у проекта новый чат сразу видит его базу знаний
  refreshConversationList(); // снимет подсветку (нового ещё нет в списке)
  inputEl.focus();
}

// Своё модальное подтверждение (нативный confirm() в Tauri-окне не работает).
function confirmModal(message: string, okLabel = "Удалить"): Promise<boolean> {
  return new Promise((resolve) => {
    const overlay = document.createElement("div");
    overlay.className = "modal-overlay";
    overlay.innerHTML = `
      <div class="modal">
        <div class="modal__text"></div>
        <div class="modal__actions">
          <button class="modal__btn" data-act="cancel">Отмена</button>
          <button class="modal__btn modal__btn--danger" data-act="ok"></button>
        </div>
      </div>`;
    overlay.querySelector(".modal__text")!.textContent = message;
    overlay.querySelector('[data-act="ok"]')!.textContent = okLabel;
    document.body.appendChild(overlay);

    const close = (result: boolean) => {
      overlay.remove();
      document.removeEventListener("keydown", onKey);
      resolve(result);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") close(false);
      else if (e.key === "Enter") close(true);
    };
    overlay.addEventListener("click", (e) => {
      const t = e.target as HTMLElement;
      if (t === overlay || t.dataset.act === "cancel") close(false);
      else if (t.dataset.act === "ok") close(true);
    });
    document.addEventListener("keydown", onKey);
    (overlay.querySelector('[data-act="ok"]') as HTMLButtonElement).focus();
  });
}

// Модальный ввод строки (нативный prompt() в Tauri-окне не работает). Возвращает
// введённую строку или null при отмене.
function promptModal(title: string, placeholder = ""): Promise<string | null> {
  return new Promise((resolve) => {
    const overlay = document.createElement("div");
    overlay.className = "modal-overlay";
    overlay.innerHTML = `
      <div class="modal">
        <div class="modal__text"></div>
        <input class="modal__input" type="text" />
        <div class="modal__actions">
          <button class="modal__btn" data-act="cancel">Отмена</button>
          <button class="modal__btn modal__btn--primary" data-act="ok">Создать</button>
        </div>
      </div>`;
    overlay.querySelector(".modal__text")!.textContent = title;
    const input = overlay.querySelector(".modal__input") as HTMLInputElement;
    input.placeholder = placeholder;
    document.body.appendChild(overlay);

    const close = (result: string | null) => {
      overlay.remove();
      document.removeEventListener("keydown", onKey);
      resolve(result);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") close(null);
      else if (e.key === "Enter") close(input.value);
    };
    overlay.addEventListener("click", (e) => {
      const t = e.target as HTMLElement;
      if (t === overlay || t.dataset.act === "cancel") close(null);
      else if (t.dataset.act === "ok") close(input.value);
    });
    document.addEventListener("keydown", onKey);
    input.focus();
  });
}

// Удаление диалога (с подтверждением — потеря данных необратима).
async function deleteConversation(id: string) {
  if (!(await confirmModal("Удалить этот диалог? Действие необратимо."))) return;
  try {
    await invoke("delete_conversation", { id });
  } catch (e) {
    addError(`Не удалось удалить диалог: ${e}`);
    return;
  }
  if (id === currentId) {
    newDialog(currentProjectId);
  } else {
    await refreshConversationList();
  }
  if (!projectView.hidden && viewingProjectId) renderProjectChats(viewingProjectId);
}

// Очистка всех диалогов (вся история стирается с диска).
async function clearAllConversations() {
  if (
    !(await confirmModal(
      "Удалить ВСЕ диалоги? Вся история будет стёрта безвозвратно.",
      "Удалить всё",
    ))
  )
    return;
  try {
    await invoke("clear_conversations");
  } catch (e) {
    addError(`Не удалось очистить диалоги: ${e}`);
    return;
  }
  newDialog(); // начинаем с чистого листа
}

// При старте: открыть самый свежий диалог или начать пустой.
async function initConversations() {
  let metas: ConversationMeta[] = [];
  try {
    metas = await invoke<ConversationMeta[]>("list_conversations");
  } catch {
    metas = [];
  }
  if (metas.length > 0) {
    await openConversation(metas[0].id); // свежий сверху
  }
  // Нет диалогов ИЛИ не удалось открыть (currentId не выставился) — начинаем новый,
  // иначе следующие сообщения молча не сохранятся (persist требует currentId).
  if (!currentId) {
    currentId = crypto.randomUUID();
    await refreshConversationList();
  }
  updateChatContextChip(); // показать чип проекта, если открылся чат внутри проекта
  refreshEmptyState(); // история загружена — теперь решаем, показывать ли приветствие
}

// Тянет список установленных моделей из Ollama (через Rust-команду list_models)
// и заполняет выпадающий список в шапке.
async function loadModels() {
  let models: ModelInfo[];
  try {
    models = await invoke<ModelInfo[]>("list_models");
  } catch (err) {
    showModelHint("Ollama недоступна");
    addError(`Не удалось получить список моделей: ${humanError(err)}`);
    return;
  }

  if (models.length === 0) {
    showModelHint("Модели не установлены");
    addError(
      "Модели не установлены. Установите модель командой, например: ollama pull qwen3.5:9b",
    );
    return;
  }

  thinkingByModel.clear();
  visionByModel.clear();
  toolsByModel.clear();
  modelSelectEl.innerHTML = "";
  for (const m of models) {
    thinkingByModel.set(m.name, m.thinking);
    visionByModel.set(m.name, m.vision);
    toolsByModel.set(m.name, m.tools);
    const opt = document.createElement("option");
    opt.value = m.name;
    opt.textContent = m.name;
    modelSelectEl.appendChild(opt);
  }
  // Предпочитаем целевую базовую модель, если она установлена; иначе — первую.
  const names = models.map((m) => m.name);
  // Сохраняем текущий выбор пользователя при «Обновить»; при старте (selectedModel
  // пуст) восстанавливаем сохранённую модель из settings.json; иначе целевая/первая.
  if (!selectedModel || !names.includes(selectedModel)) {
    let saved: string | null = null;
    if (!selectedModel) {
      try {
        saved = await invoke<string | null>("get_setting", { key: "selected_model" });
      } catch {
        saved = null;
      }
    }
    if (saved && names.includes(saved)) {
      selectedModel = saved; // сохранённая модель ещё установлена
    } else {
      const preferred = "qwen3.5:9b"; // откат: целевая, иначе первая в списке
      selectedModel = names.includes(preferred) ? preferred : names[0];
    }
  }
  modelSelectEl.value = selectedModel;
  modelSelectEl.disabled = false;
  setComposerEnabled(true);
  updateThinkAvailability();
  inputEl.focus();
}

// Включает/выключает тумблер «Размышления» по возможностям выбранной модели.
function updateThinkAvailability() {
  const supports = thinkingByModel.get(selectedModel) ?? false;
  thinkToggleEl.disabled = !supports;
  thinkToggleEl.title = supports
    ? "Режим рассуждений модели (медленнее, но точнее)"
    : "Эта модель не поддерживает режим рассуждений";
  thinkToggleEl.classList.toggle("on", supports && thinkEnabled);
}

// Сведения о железе (из Rust-команды detect_hardware).
interface HardwareInfo {
  ram_gb: number;
  cpu_cores: number;
  vram_gb: number | null; // всего (класс железа)
  vram_free_gb: number | null; // свободно сейчас (честная доступность)
  vram_source: string;
  tier: "green" | "yellow" | "red";
}

// Склонение существительного по числу (русские правила): 1 ядро, 4 ядра, 18 ядер.
function plural(n: number, one: string, few: string, many: string): string {
  const m10 = n % 10;
  const m100 = n % 100;
  if (m10 === 1 && m100 !== 11) return one;
  if (m10 >= 2 && m10 <= 4 && (m100 < 12 || m100 > 14)) return few;
  return many;
}

// «Светофор» железа: словесная оценка + характеристики с подписями. Уровень,
// числа и рекомендуемую модель берём из detect_hardware (логику НЕ меняем).
async function loadHardware() {
  const wordEl = document.querySelector("#hw-word")!;
  const specsEl = document.querySelector("#hw-specs")!;
  const modelEl = document.querySelector("#hw-model") as HTMLElement;
  const modelNameEl = document.querySelector("#hw-model-name")!;
  let hw: HardwareInfo;
  try {
    hw = await invoke<HardwareInfo>("detect_hardware");
  } catch {
    wordEl.textContent = "Конфигурация не определена";
    specsEl.textContent = "";
    modelEl.hidden = true;
    hwBarEl.className = "hwchip hwchip--unknown";
    hwBarEl.hidden = false;
    return;
  }

  // Цвет — у кружка (классы hwchip--*); статус и рекомендованная модель — текстом в блоке.
  const word =
    hw.tier === "green" ? "Оптимально" : hw.tier === "yellow" ? "Достаточно" : "Ограничено";
  const model = hw.tier === "green" ? "qwen3.5:9b" : "qwen3.5:4b";

  // Внутри характеристики — неразрывные пробелы (не рвётся); перенос только между ними.
  const nb = " ";
  const specs: string[] = [];
  // GPU — только при наличии выделенной видеопамяти (на unified/Apple Silicon vram_gb == null).
  // Рядом — честная свободная сейчас (если ОС её сообщает), а не только паспортный объём.
  if (hw.vram_gb != null) {
    const free = hw.vram_free_gb != null ? ` (свободно${nb}${hw.vram_free_gb.toFixed(1)})` : "";
    specs.push(`GPU${nb}${hw.vram_gb.toFixed(0)}${nb}ГБ${free}`);
  }
  specs.push(`RAM${nb}${hw.ram_gb.toFixed(0)}${nb}ГБ`);
  specs.push(`CPU${nb}${hw.cpu_cores}${nb}${plural(hw.cpu_cores, "ядро", "ядра", "ядер")}`);

  wordEl.textContent = word; // строка 1: статус
  specsEl.textContent = specs.join(" · "); // строка 2: характеристики (перенос по « · »)
  modelNameEl.textContent = `Рекомендуется ${model}`; // строка 3: модель — прямо в блоке
  modelEl.hidden = false;
  hwBarEl.className = `hwchip hwchip--${hw.tier}`;
  hwBarEl.hidden = false;
}

// Обеспечить движок при старте: приложение само переиспользует запущенную Ollama
// или поднимает свою (терминал пользователю не нужен). Возвращает, готов ли движок.
async function ensureEngine(): Promise<boolean> {
  const engine = document.querySelector("#engine")!;
  statusEl.textContent = "Запуск движка…";
  engine.classList.remove("engine--down");
  let res: { status: string; message: string };
  try {
    res = await invoke("ensure_engine");
  } catch (e) {
    statusEl.textContent = "Движок недоступен";
    engine.classList.add("engine--down");
    console.error("ensure_engine:", e);
    return false;
  }
  if (res.status === "ready") return true; // checkOllama ниже покажет версию
  // not_installed / error — показываем понятный статус, дальнейшие шаги пропустим
  statusEl.textContent =
    res.status === "not_installed" ? "Движок не установлен" : "Движок не запущен";
  engine.classList.add("engine--down");
  return false;
}

// Мягкая проверка движка: спрашиваем версию Ollama и показываем её в шапке.
// Неблокирующая — при недоступности просто показываем статус, приложение работает.
async function checkOllama() {
  const engine = document.querySelector("#engine")!;
  try {
    const version = await invoke<string>("ollama_version");
    statusEl.textContent = `Ollama ${version}`;
    engine.classList.remove("engine--down");
  } catch {
    statusEl.textContent = "Ollama недоступна";
    engine.classList.add("engine--down");
  }
}

// Кнопка «обновить»: заново проверяем движок и перечитываем список моделей,
// чтобы подхватить только что скачанные модели без перезапуска приложения.
async function refreshAll() {
  refreshBtn.disabled = true;
  try {
    await checkOllama();
    await loadModels();
  } finally {
    refreshBtn.disabled = false;
  }
}

// «Проверка»: полная перепроверка движка, железа и списка моделей.
async function recheck() {
  checkBtn.disabled = true;
  try {
    await checkOllama();
    await Promise.all([loadHardware(), loadModels()]);
  } finally {
    checkBtn.disabled = false;
  }
}

// Показывает в списке одиночную подсказку и блокирует ввод (нет моделей / нет Ollama).
function showModelHint(text: string) {
  modelSelectEl.innerHTML = "";
  const opt = document.createElement("option");
  opt.textContent = text;
  modelSelectEl.appendChild(opt);
  modelSelectEl.disabled = true;
  selectedModel = "";
  setComposerEnabled(false);
}

// ── Тема (светлая/тёмная). Выбор хранится через Tauri (settings.json) ────────

// Иконки: показываем действие-противоположность (в тёмной — солнце, в светлой — луна).
const ICON_SUN =
  '<circle cx="12" cy="12" r="4"/><path d="M12 2v3M12 19v3M2 12h3M19 12h3M5 5l2 2M17 17l2 2M19 5l-2 2M7 17l-2 2"/>';
const ICON_MOON = '<path d="M21 12.8A9 9 0 1 1 11.2 3a7 7 0 0 0 9.8 9.8z"/>';

function applyTheme(theme: string) {
  document.documentElement.setAttribute("data-theme", theme);
  const icon = document.querySelector("#theme-icon");
  if (icon) icon.innerHTML = theme === "dark" ? ICON_SUN : ICON_MOON;
}

// При старте: тема из настроек Tauri; если не сохранена — по системной.
async function initTheme() {
  let theme: string | null = null;
  try {
    theme = await invoke<string | null>("get_setting", { key: "theme" });
  } catch {
    theme = null;
  }
  if (theme !== "light" && theme !== "dark") {
    theme = window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
  }
  applyTheme(theme);
  // Тема применена — включаем переходы (чтобы интерфейс не «переплывал» на старте).
  requestAnimationFrame(() => document.documentElement.classList.remove("no-transitions"));
}

function toggleTheme() {
  const next =
    document.documentElement.getAttribute("data-theme") === "dark" ? "light" : "dark";
  applyTheme(next);
  invoke("set_setting", { key: "theme", value: next }).catch((e) =>
    console.error("set_setting:", e),
  );
}

// Восстановление тумблера «Размышления» из settings.json (единый источник истины).
// По умолчанию ВЫКЛ: с reasoning-моделью даже простые вопросы думаются по ~20 секунд.
// Одноразовая миграция из прежнего хранилища localStorage("jai.think"), чтобы выбор
// пользователя не потерялся; затем localStorage для этой настройки не используется.
async function initThinking() {
  let saved: string | null = null;
  try {
    saved = await invoke<string | null>("get_setting", { key: "thinking_enabled" });
  } catch {
    saved = null;
  }
  if (saved === null) {
    const legacy = localStorage.getItem("jai.think"); // прежнее хранилище
    if (legacy !== null) {
      saved = legacy; // "true"/"false"
      invoke("set_setting", { key: "thinking_enabled", value: saved }).catch((e) =>
        console.error("set_setting thinking_enabled (миграция):", e),
      );
      localStorage.removeItem("jai.think"); // дальше — только settings.json
    }
  }
  thinkEnabled = saved === "true"; // null/"false" → ВЫКЛ
  thinkToggleEl.classList.toggle("on", thinkEnabled);
  thinkToggleEl.addEventListener("click", () => {
    thinkEnabled = !thinkEnabled;
    thinkToggleEl.classList.toggle("on", thinkEnabled);
    invoke("set_setting", { key: "thinking_enabled", value: String(thinkEnabled) }).catch((e) =>
      console.error("set_setting thinking_enabled:", e),
    );
  });
}

// ── Онлайн-режим (агентный, tool calling) ────────────────────────────────────

// Применяет состояние онлайн-режима ко всему UI: тумблер в композере, индикатор в
// шапке (виден только в онлайне — в офлайне обычная работа), подпись и кнопка в
// настройках. `persist` — записать выбор в settings.json (по умолчанию да).
function setOnlineMode(on: boolean, persist = true) {
  onlineMode = on;
  onlineToggleEl.classList.toggle("on", on);
  onlineBadgeEl.hidden = !on; // индикатор «данные могут уходить наружу» только в онлайне
  onlineStateTextEl.textContent = on ? "включён" : "выключен";
  onlineMasterToggleEl.textContent = on ? "Выключить" : "Включить";
  if (persist) {
    invoke("set_setting", { key: "online_mode", value: on ? "1" : "0" }).catch((e) =>
      console.error("set_setting online_mode:", e),
    );
  }
}

// Восстанавливает онлайн-режим и настройки веб-поиска из settings.json и навешивает
// обработчики. Онлайн ВЫКЛЮЧЕН, пока пользователь явно не включал (152-ФЗ).
async function initOnline() {
  // Состояние режима (по умолчанию выкл).
  let onlineSaved: string | null = null;
  try {
    onlineSaved = await invoke<string | null>("get_setting", { key: "online_mode" });
  } catch {
    onlineSaved = null;
  }
  setOnlineMode(onlineSaved === "1", false); // только "1" = включён; иначе офлайн

  // Поля провайдера/эндпоинта/ключа (дефолты — Tavily; пустой ключ = поиск недоступен).
  const load = async (key: string) => {
    try {
      return (await invoke<string | null>("get_setting", { key })) ?? "";
    } catch {
      return "";
    }
  };
  wsProviderEl.value = (await load("web_search_provider")) || "tavily";
  wsUrlEl.value = (await load("web_search_url")) || "https://api.tavily.com/search";
  wsKeyEl.value = await load("web_search_api_key");

  // Сохраняем поля по уходу фокуса (как другие настройки — единый источник settings.json).
  const saveField = (el: HTMLInputElement, key: string) => {
    el.addEventListener("change", () => {
      invoke("set_setting", { key, value: el.value.trim() }).catch((e) =>
        console.error(`set_setting ${key}:`, e),
      );
    });
  };
  saveField(wsProviderEl, "web_search_provider");
  saveField(wsUrlEl, "web_search_url");
  saveField(wsKeyEl, "web_search_api_key");

  // Тумблеры (композер + мастер в настройках) переключают один и тот же режим.
  onlineToggleEl.addEventListener("click", () => setOnlineMode(!onlineMode));
  onlineMasterToggleEl.addEventListener("click", () => setOnlineMode(!onlineMode));

  // Журнал исходящих обращений.
  outboundRefreshBtn.addEventListener("click", refreshOutboundLog);
  outboundClearBtn.addEventListener("click", async () => {
    try {
      await invoke("clear_outbound_log");
      await refreshOutboundLog();
    } catch (e) {
      showOnlineStatus(`Не удалось очистить журнал: ${e}`);
    }
  });
}

// Запись журнала исходящих обращений (зеркало OutboundLogEntry в lib.rs).
interface OutboundLogEntry {
  ts: number;
  host: string;
  tool: string;
  query: string;
}

// Подтягивает журнал обращений в интернет и рисует его (новые сверху). Прозрачность
// постфактум: что и на какой хост ушло (152-ФЗ при отсутствии пер-запросного запроса).
async function refreshOutboundLog() {
  let entries: OutboundLogEntry[] = [];
  try {
    entries = await invoke<OutboundLogEntry[]>("get_outbound_log");
  } catch (e) {
    outboundLogEl.textContent = `Не удалось загрузить журнал: ${e}`;
    return;
  }
  outboundLogEl.innerHTML = "";
  if (!entries.length) {
    const empty = document.createElement("div");
    empty.className = "online-log__empty";
    empty.textContent = "Обращений в интернет пока не было.";
    outboundLogEl.appendChild(empty);
    return;
  }
  for (const e of entries) {
    const row = document.createElement("div");
    row.className = "online-log__row";
    const when = new Date(e.ts).toLocaleString();
    const meta = document.createElement("div");
    meta.className = "online-log__meta";
    meta.textContent = `${when} · ${e.host} · ${e.tool}`;
    const q = document.createElement("div");
    q.className = "online-log__query";
    q.textContent = e.query ? `«${e.query}»` : "";
    row.append(meta, q);
    outboundLogEl.appendChild(row);
  }
}

function showOnlineStatus(text: string) {
  onlineStatusEl.textContent = text;
  onlineStatusEl.hidden = false;
}

// Авто-высота поля ввода под текст.
function autoGrow() {
  inputEl.style.height = "auto";
  inputEl.style.height = Math.min(inputEl.scrollHeight, 200) + "px";
}

window.addEventListener("DOMContentLoaded", async () => {
  messagesEl = document.querySelector("#messages")!;
  feedEl = document.querySelector("#feed")!;
  emptyStateEl = document.querySelector("#empty-state")!;
  inputEl = document.querySelector("#chat-input")!;
  sendBtn = document.querySelector("#send-btn")!;
  stopBtn = document.querySelector("#stop-btn")!;
  modelSelectEl = document.querySelector("#model-select")!;
  statusEl = document.querySelector("#status")!;
  refreshBtn = document.querySelector("#refresh-btn")!;
  hwBarEl = document.querySelector("#hw-bar")!;
  convListEl = document.querySelector("#conv-list")!;
  newChatBtn = document.querySelector("#new-chat-btn")!;
  thinkToggleEl = document.querySelector("#think-toggle")!;
  onlineToggleEl = document.querySelector("#online-toggle")!;
  onlineBadgeEl = document.querySelector("#online-badge")!;
  onlineStateTextEl = document.querySelector("#online-state-text")!;
  onlineMasterToggleEl = document.querySelector("#online-master-toggle")!;
  wsProviderEl = document.querySelector("#ws-provider")!;
  wsUrlEl = document.querySelector("#ws-url")!;
  wsKeyEl = document.querySelector("#ws-key")!;
  outboundLogEl = document.querySelector("#outbound-log")!;
  outboundRefreshBtn = document.querySelector("#outbound-refresh")!;
  outboundClearBtn = document.querySelector("#outbound-clear")!;
  onlineStatusEl = document.querySelector("#online-status")!;
  themeBtn = document.querySelector("#theme-btn")!;
  settingsBtn = document.querySelector("#settings-btn")!;
  settingsView = document.querySelector("#settings-view")!;
  settingsBackBtn = document.querySelector("#settings-back")!;
  composerWrapEl = document.querySelector("#chat-form")!;
  appEl = document.querySelector(".app")!;
  sidebarResizer = document.querySelector("#sidebar-resizer")!;
  sidebarToggleBtn = document.querySelector("#sidebar-toggle")!;
  convSearchEl = document.querySelector("#conv-search")!;
  checkBtn = document.querySelector("#check-btn")!;
  clearBtn = document.querySelector("#clear-btn")!;
  attachBtn = document.querySelector("#attach-btn")!;
  docChipEl = document.querySelector("#doc-chip")!;
  docChipBadgeEl = document.querySelector("#doc-chip-badge")!;
  docChipNameEl = document.querySelector("#doc-chip-name")!;
  docRemoveBtn = document.querySelector("#doc-remove")!;
  imageBtn = document.querySelector("#image-btn")!;
  imgChipEl = document.querySelector("#img-chip")!;
  imgChipThumb = document.querySelector("#img-chip-thumb")!;
  imgOcrBtn = document.querySelector("#img-ocr")!;
  imgRemoveBtn = document.querySelector("#img-remove")!;
  attachBtn.addEventListener("click", attachDocument);
  docRemoveBtn.addEventListener("click", removeDocument);
  imageBtn.addEventListener("click", attachImage);
  imgRemoveBtn.addEventListener("click", removeImage);
  imgOcrBtn.addEventListener("click", ocrImage);
  tabChatsBtn = document.querySelector("#tab-chats-btn")!;
  tabDocsBtn = document.querySelector("#tab-docs-btn")!;
  paneChatsEl = document.querySelector("#pane-chats")!;
  paneDocsEl = document.querySelector("#pane-docs")!;
  addDocBtn = document.querySelector("#add-doc-btn")!;
  docListEl = document.querySelector("#doc-list")!;
  docStatusEl = document.querySelector("#doc-status")!;
  docStatusTextEl = document.querySelector("#doc-status-text")!;
  installEmbedBtn = document.querySelector("#install-embed-btn")!;
  installLocalBtn = document.querySelector("#install-local-btn")!;
  pullCancelBtn = document.querySelector("#pull-cancel-btn")!;
  epModelsEl = document.querySelector("#ep-models")!;
  epEngineEl = document.querySelector("#ep-engine")!;
  epSetModelsBtn = document.querySelector("#ep-set-models")!;
  epSetEngineBtn = document.querySelector("#ep-set-engine")!;
  epResetBtn = document.querySelector("#ep-reset")!;
  settingsStatusEl = document.querySelector("#settings-status")!;
  modelListEl = document.querySelector("#model-list")!;
  checkUpdatesBtn = document.querySelector("#check-updates-btn")!;
  installFromDiskBtn = document.querySelector("#install-from-disk-btn")!;
  modelsStatusEl = document.querySelector("#models-status")!;
  modelProgressEl = document.querySelector("#model-progress")!;
  modelProgressFill = document.querySelector("#model-progress-fill")!;
  modelProgressLabel = document.querySelector("#model-progress-label")!;
  modelPullCancelBtn = document.querySelector("#model-pull-cancel")!;
  checkUpdatesBtn.addEventListener("click", checkModelUpdates);
  installFromDiskBtn.addEventListener("click", installFromDiskForModels);
  modelPullCancelBtn.addEventListener("click", cancelActivePull);
  diagRunBtn = document.querySelector("#diag-run-btn")!;
  diagListEl = document.querySelector("#diag-list")!;
  diagRunBtn.addEventListener("click", runDiagnostics);
  appUpdateCheckBtn = document.querySelector("#app-update-check")!;
  appUpdateDiskBtn = document.querySelector("#app-update-disk")!;
  appUpdateInfoEl = document.querySelector("#app-update-info")!;
  appUpdateProgressEl = document.querySelector("#app-update-progress")!;
  appUpdateProgressFill = document.querySelector("#app-update-progress-fill")!;
  appUpdateProgressLabel = document.querySelector("#app-update-progress-label")!;
  appUpdateStatusEl = document.querySelector("#app-update-status")!;
  appVersionEl = document.querySelector("#app-version")!;
  appUpdateCheckBtn.addEventListener("click", checkAppUpdate);
  appUpdateDiskBtn.addEventListener("click", installAppUpdateFromDisk);
  getVersion()
    .then((v) => (appVersionEl.textContent = v))
    .catch(() => (appVersionEl.textContent = "—"));
  indexProgressEl = document.querySelector("#index-progress")!;
  indexProgressFill = document.querySelector("#index-progress-fill")!;
  indexProgressLabel = document.querySelector("#index-progress-label")!;
  // Проекты: элементы боковой панели и экрана проекта.
  projectListEl = document.querySelector("#project-list")!;
  newProjectBtn = document.querySelector("#new-project-btn")!;
  projectView = document.querySelector("#project-view")!;
  projectBackBtn = document.querySelector("#project-back")!;
  projectNameInput = document.querySelector("#project-name-input")!;
  projectDeleteBtn = document.querySelector("#project-delete")!;
  projectInstructionsEl = document.querySelector("#project-instructions")!;
  projectStatusEl = document.querySelector("#project-status")!;
  projectAddDocBtn = document.querySelector("#project-add-doc")!;
  projectDocListEl = document.querySelector("#project-doc-list")!;
  projectChatListEl = document.querySelector("#project-chat-list")!;
  projectIndexProgressEl = document.querySelector("#project-index-progress")!;
  projectIndexProgressFill = document.querySelector("#project-index-progress-fill")!;
  projectIndexProgressLabel = document.querySelector("#project-index-progress-label")!;
  chatProjectChip = document.querySelector("#chat-project-chip")!;
  chatProjectChipName = document.querySelector("#chat-project-chip-name")!;

  // Контексты документов: сайдбар (общие, вне проектов) и экран проекта.
  sidebarDocCtx = {
    projectId: null,
    listEl: docListEl,
    progressEl: indexProgressEl,
    fillEl: indexProgressFill,
    labelEl: indexProgressLabel,
    addBtn: addDocBtn,
    flashTimer: null,
    opGen: 0,
  };
  projectDocCtx = {
    projectId: null,
    listEl: projectDocListEl,
    progressEl: projectIndexProgressEl,
    fillEl: projectIndexProgressFill,
    labelEl: projectIndexProgressLabel,
    addBtn: projectAddDocBtn,
    flashTimer: null,
    opGen: 0,
  };

  tabChatsBtn.addEventListener("click", () => switchTab("chats"));
  tabDocsBtn.addEventListener("click", () => switchTab("docs"));
  addDocBtn.addEventListener("click", () => addDocument(sidebarDocCtx));
  newProjectBtn.addEventListener("click", createProject);
  projectBackBtn.addEventListener("click", closeProjectView);
  projectDeleteBtn.addEventListener("click", deleteProjectFlow);
  projectAddDocBtn.addEventListener("click", () => addDocument(projectDocCtx));
  document.querySelector("#project-new-chat")!.addEventListener("click", () => {
    if (viewingProjectId) newDialog(viewingProjectId);
  });
  chatProjectChip.addEventListener("click", () => {
    if (currentProjectId) openProjectView(currentProjectId);
  });
  // Имя проекта — сохраняем при потере фокуса/Enter.
  const commitProjectName = () => {
    if (!viewingProjectId) return;
    const proj = projects.find((p) => p.id === viewingProjectId);
    if (!proj) return;
    const name = projectNameInput.value.trim() || "Без названия";
    if (name !== proj.name) {
      proj.name = name;
      saveProjectMeta(proj);
    }
  };
  projectNameInput.addEventListener("blur", commitProjectName);
  projectNameInput.addEventListener("keydown", (e) => {
    if (e.key === "Enter") projectNameInput.blur();
  });
  // Инструкции проекта — сохраняем при потере фокуса.
  projectInstructionsEl.addEventListener("blur", () => {
    if (!viewingProjectId) return;
    const proj = projects.find((p) => p.id === viewingProjectId);
    if (!proj) return;
    if (projectInstructionsEl.value !== proj.instructions) {
      proj.instructions = projectInstructionsEl.value;
      saveProjectMeta(proj);
      projectStatus("Инструкции сохранены", false);
    }
  });
  installEmbedBtn.addEventListener("click", installEmbeddingModel);
  installLocalBtn.addEventListener("click", installFromLocalDir);
  pullCancelBtn.addEventListener("click", cancelActivePull);
  epSetModelsBtn.addEventListener("click", settingsPickModels);
  epSetEngineBtn.addEventListener("click", setEnginePathDialog);
  epResetBtn.addEventListener("click", resetEnginePaths);
  modelSelectEl.addEventListener("change", () => {
    selectedModel = modelSelectEl.value;
    updateThinkAvailability(); // у новой модели могут быть другие возможности
    // запоминаем выбор между запусками (settings.json — единый источник истины)
    invoke("set_setting", { key: "selected_model", value: selectedModel }).catch((e) =>
      console.error("set_setting selected_model:", e),
    );
  });
  refreshBtn.addEventListener("click", refreshAll);
  newChatBtn.addEventListener("click", () => newDialog(null));
  themeBtn.addEventListener("click", toggleTheme);
  settingsBtn.addEventListener("click", openSettings);
  settingsBackBtn.addEventListener("click", closeSettings);
  sidebarResizer.addEventListener("pointerdown", startSidebarResize);
  sidebarToggleBtn.addEventListener("click", toggleSidebar);
  checkBtn.addEventListener("click", recheck);
  clearBtn.addEventListener("click", clearAllConversations);
  convSearchEl.addEventListener("input", () => {
    convFilter = convSearchEl.value;
    renderConvList();
  });

  // Кнопка «Копировать» в код-блоках (через делегирование).
  messagesEl.addEventListener("click", (e) => {
    const btn = (e.target as HTMLElement).closest(".copy") as HTMLButtonElement | null;
    if (!btn) return;
    const pre = btn.closest(".code")?.querySelector("pre");
    if (!pre) return;
    const restore = () => setTimeout(() => (btn.textContent = "Копировать"), 1500);
    navigator.clipboard
      .writeText(pre.textContent || "")
      .then(() => {
        btn.textContent = "✓ Скопировано";
        restore();
      })
      .catch(() => {
        btn.textContent = "Не удалось";
        restore();
      });
  });

  // Авто-следование за ответом включаем/выключаем по позиции прокрутки.
  feedEl.addEventListener("scroll", () => {
    autoScroll = feedEl.scrollHeight - feedEl.scrollTop - feedEl.clientHeight < 80;
  });

  // Чипы пустого состояния: подставляют текст в поле (без автоотправки).
  emptyStateEl.addEventListener("click", (e) => {
    const chip = (e.target as HTMLElement).closest(".chip") as HTMLButtonElement | null;
    if (!chip) return;
    inputEl.value = chip.dataset.prompt || chip.textContent?.trim() || "";
    autoGrow(); // существующий авто-ресайз поля
    inputEl.focus();
  });

  initTheme(); // применяем сохранённую/системную тему как можно раньше
  initThinking(); // восстанавливаем тумблер «Размышления» из настроек (+миграция)
  initOnline(); // восстанавливаем онлайн-режим и настройки веб-поиска (по умолчанию офлайн)
  initSidebar(); // восстанавливаем ширину и состояние левой панели

  document.querySelector("#chat-form")?.addEventListener("submit", (e) => {
    e.preventDefault();
    send();
  });
  stopBtn.addEventListener("click", stop);

  inputEl.addEventListener("input", autoGrow);
  inputEl.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      send();
    }
  });

  setComposerEnabled(false); // включим, когда загрузится список моделей
  await refreshProjects();   // список проектов в боковой панели
  await initConversations(); // сначала восстановим диалоги в ленту
  loadHardware();            // неблокирующе: светофор железа (локально, без движка)
  // Сначала обеспечиваем движок (поднимаем свой или переиспользуем системный), затем
  // уже опираемся на него. Если не готов — статус выставлен, движок-зависимые шаги
  // пропускаем (пользователь может повторить кнопкой «Проверка»).
  const engineReady = await ensureEngine();
  if (engineReady) {
    checkOllama();           // покажет версию Ollama в шапке
    loadModels();
    refreshDocuments(sidebarDocCtx);      // общая база (вне проектов) + статус модели эмбеддингов
    refreshCurrentDocsCount();            // есть ли документы у открытого чата (для RAG)
  }
});
