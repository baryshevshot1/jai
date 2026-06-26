import { invoke, Channel } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
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

// Прикреплённый документ (Фаза A). text уже усечён под бюджет контекста.
// ~50% от num_ctx 8192 (≈3 симв/токен) — остаётся место под систему/историю/ответ.
const DOC_CHAR_BUDGET = 12000;

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
const CONTEXT_CHAR_BUDGET = 7000; // ~2300 токенов — с запасом под систему/вопрос/ответ
const RAG_HISTORY_LIMIT = 6; // последних сообщений истории при активном поиске

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
// Прогресс установки модели (Channel из pull_model).
type PullEvent =
  | { type: "progress"; status: string; completed: number; total: number }
  | { type: "done" };

// Число документов в базе: >0 → перед ответом ищем релевантные фрагменты.
let docsCount = 0;
// Установлена ли модель эмбеддингов (без неё индексация/поиск невозможны).
let embeddingReady = false;
// Идёт ли установка модели (pull) и была ли она отменена пользователем.
let pulling = false;
let pullCancelled = false;

// Документы-источники, уже показанные в строке «Источники» в текущем диалоге.
// Каждый документ упоминаем один раз — дальше не повторяем заметку под ответами.
const shownSourceFiles = new Set<string>();

// События из Rust (см. ChatEvent в lib.rs).
type ChatEvent =
  | { type: "chunk"; content: string }
  | { type: "thinking"; content: string }
  | { type: "done" };

type Role = "user" | "assistant";
interface Message {
  role: Role;
  content: string;
  doc?: DocAttachment; // только у реплик пользователя, к которым приложен файл
  sources?: SourceRef[]; // только у ответов ассистента на основе базы документов
  images?: string[]; // base64 прикреплённых изображений (зрение, qwen3-vl)
}

// Сообщение для Ollama: role/content (+ опц. images у vision-запросов).
type ApiMsg = { role: string; content: string; images?: string[] };

// Модель + поддержка рассуждений и зрения (из list_models).
interface ModelInfo {
  name: string;
  thinking: boolean;
  vision: boolean;
}
// Какие модели поддерживают «Размышления» / «Зрение» (имя → bool).
const thinkingByModel = new Map<string, boolean>();
const visionByModel = new Map<string, boolean>();

// История ОТКРЫТОГО диалога (без системного сообщения — его добавляем при отправке).
const history: Message[] = [];

// id текущего диалога (файл appDataDir/conversations/<id>.json).
let currentId = "";

// Кэш списка диалогов и текущий фильтр поиска (для отрисовки боковой панели).
let convMetas: ConversationMeta[] = [];
let convFilter = "";

// Карточка диалога для боковой панели и полный диалог из файла.
interface ConversationMeta {
  id: string;
  title: string;
  updated_at: number;
}
interface Conversation {
  id: string;
  title: string;
  updated_at: number;
  messages: Message[];
}

// Счётчик «поколений»: позволяет кнопке «Стоп» игнорировать поздние кусочки.
let generation = 0;
let streaming = false;
// Следовать за ответом только если пользователь у низа ленты (иначе не мешаем читать).
let autoScroll = true;

// Режим рассуждений (тумблер). По умолчанию включён; выбор хранится в localStorage.
let thinkEnabled = true;

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
let indexProgressEl: HTMLElement;
let indexProgressFill: HTMLElement;
let indexProgressLabel: HTMLElement;

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

// Собирает массив сообщений для Ollama: система + история (+ контекст из базы).
// Реплику с приложенным документом разворачиваем в текст «документ + вопрос»; сам
// объект doc/sources в запрос не попадает — только чистые {role, content}.
// contextMsg (если есть) — фрагменты из базы, вставляются ПЕРЕД текущим вопросом;
// при активном поиске историю урезаем — место в num_ctx занимают фрагменты.
function buildApiMessages(contextMsg: Message | null): ApiMsg[] {
  const out: ApiMsg[] = [SYSTEM];
  const msgs = contextMsg ? history.slice(-RAG_HISTORY_LIMIT) : history;
  for (let i = 0; i < msgs.length; i++) {
    const m = msgs[i];
    if (contextMsg && i === msgs.length - 1 && m.role === "user") {
      out.push({ role: contextMsg.role, content: contextMsg.content });
    }
    let item: ApiMsg;
    if (m.role === "user" && m.doc) {
      // Документ — справочный материал к этому ходу. НЕ приказываем «отвечай только
      // по нему» и не просим упоминать его в каждом ответе: модель обращается к файлу,
      // когда это относится к вопросу, и не пересказывает его постоянно (как делают
      // другие ассистенты). Содержимое остаётся в контексте диалога для follow-up.
      item = {
        role: "user",
        content:
          `[Прикреплён документ «${m.doc.name}» — справочный материал, ` +
          `обращайся к нему, когда это относится к вопросу]\n\n` +
          `${m.doc.text}\n\n———\n\n${m.content}`,
      };
    } else {
      item = { role: m.role, content: m.content };
    }
    // Изображения (зрение) — сырой base64 в поле images; остаются в контексте для follow-up.
    if (m.role === "user" && m.images && m.images.length) {
      item.images = m.images;
    }
    out.push(item);
  }
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

  const onEvent = new Channel<ChatEvent>();
  onEvent.onmessage = (msg) => {
    if (myGen !== generation) return; // нажали «Стоп» — игнорируем хвост
    if (msg.type === "thinking") {
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
  if (docsCount > 0) {
    try {
      const retrieved = await invoke<RetrievedChunk[]>("search_documents", {
        query: text,
        k: RAG_TOP_K,
      });
      if (myGen !== generation) return; // остановили во время поиска
      if (retrieved.length) {
        const built = buildContext(retrieved);
        contextMsg = built.contextMsg;
        sources = built.sources;
      }
    } catch (e) {
      if (myGen !== generation) return;
      addNotice(`Поиск по документам недоступен: ${e}`);
    }
  }

  // Контекст для модели: система + история (урезанная при RAG) + контекст из базы.
  // У реплик с приложенным файлом текст документа вшивается в ход (как в Фазе A).
  const messages = buildApiMessages(contextMsg);

  try {
    // Возвращённое значение — ПОЛНЫЙ текст ответа (без гонок с доставкой канала).
    const full = await invoke<string>("chat_stream", {
      model: selectedModel,
      messages,
      // think:true шлём ТОЛЬКО моделям, которые это поддерживают (иначе Ollama
      // вернёт ошибку «не умеет размышлять»).
      think: thinkEnabled && (thinkingByModel.get(selectedModel) ?? false),
      onEvent,
    });
    if (myGen === generation) {
      ui.thinking.remove();
      freezeReason(true);
      if (reasoning) ui.rbody.textContent = reasoning; // готов, раскрывается по клику
      answer = full; // авторитетный полный ответ
      if (answer.trim()) {
        renderAnswer(answer); // финальное форматирование один раз
        if (sources.length) renderSources(ui.turn, sources); // из каких документов взято
        history.push({
          role: "assistant",
          content: answer,
          ...(sources.length ? { sources } : {}),
        });
        persist();
      } else if (!reasoning.trim()) {
        ui.turn.remove(); // совсем пусто — убираем
      }
      scrollToBottom();
      setStreaming(false);
    }
  } catch (err) {
    if (myGen !== generation) return;
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
const VISION_MODEL = "qwen3-vl:2b"; // лёгкий вариант для зрения/OCR

async function offerInstallVision() {
  const ok = await confirmModal(
    `Для работы с изображениями нужна модель зрения. Установить ${VISION_MODEL} (~2 ГБ)? Потребуется интернет.`,
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
  row.textContent = `Установка ${VISION_MODEL}…`;
  messagesEl.appendChild(row);
  refreshEmptyState();
  scrollToBottom();

  const onEvent = new Channel<PullEvent>();
  onEvent.onmessage = (e) => {
    if (e.type !== "progress") return;
    const pct = e.total > 0 ? ` ${Math.round((e.completed / e.total) * 100)}%` : "";
    row.textContent = `Установка ${VISION_MODEL}: ${ruPullStatus(e.status)}${pct}`;
    scrollToBottom();
  };
  try {
    await invoke("pull_model", { name: VISION_MODEL, onEvent });
    row.textContent = `Модель ${VISION_MODEL} установлена — можно работать с изображениями.`;
    await loadModels();
    ensureVisionModel(); // теперь vision-модель есть → переключимся на неё
  } catch (e) {
    row.className = "err";
    row.textContent = `Не удалось установить ${VISION_MODEL}: ${e}`;
  }
}

// ── База документов (Фаза B5): вкладки сайдбара + список/добавление/удаление ──

function switchTab(tab: "chats" | "docs") {
  const docs = tab === "docs";
  paneChatsEl.hidden = docs;
  paneDocsEl.hidden = !docs;
  tabChatsBtn.classList.toggle("active", !docs);
  tabDocsBtn.classList.toggle("active", docs);
  if (docs) refreshDocuments(); // на открытии вкладки — свежий список и статус модели
}

// Тянет список документов и статус модели эмбеддингов; обновляет docsCount.
async function refreshDocuments() {
  try {
    embeddingReady = await invoke<boolean>("embedding_status");
  } catch {
    embeddingReady = false;
  }
  // нет модели эмбеддингов → карточка с кнопкой установки. Во время установки
  // карточку не показываем (на её месте — прогресс), но скрыть при готовности можно.
  if (embeddingReady) {
    docStatusEl.hidden = true;
  } else if (!pulling) {
    docStatusEl.hidden = false;
    docStatusTextEl.textContent =
      "Для поиска по документам нужна модель bge-m3. Скачайте из интернета или укажите локальную поставку (каталог моделей Ollama) — без терминала.";
    installEmbedBtn.hidden = false;
    installEmbedBtn.disabled = false;
    installEmbedBtn.textContent = "Скачать (~1.2 ГБ)";
    installLocalBtn.hidden = false;
    installLocalBtn.disabled = false;
  }

  let docs: DocumentMeta[] = [];
  try {
    docs = await invoke<DocumentMeta[]>("list_documents");
  } catch (e) {
    console.error("list_documents:", e);
  }
  docsCount = docs.length;
  renderDocList(docs);
}

function renderDocList(docs: DocumentMeta[]) {
  docListEl.innerHTML = "";
  if (docs.length === 0) {
    const empty = document.createElement("div");
    empty.className = "doc-empty";
    empty.textContent = embeddingReady
      ? "База пуста. Добавьте документы — и спрашивайте по ним."
      : "Документов пока нет.";
    docListEl.appendChild(empty);
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
    del.addEventListener("click", () => deleteDocument(d));
    item.appendChild(del);

    docListEl.appendChild(item);
  }
}

function showIndexProgress(label: string, frac: number) {
  indexProgressEl.hidden = false;
  indexProgressFill.style.width = `${Math.round(frac * 100)}%`;
  indexProgressLabel.textContent = label;
  indexProgressLabel.classList.remove("danger");
}

function hideIndexProgress() {
  indexProgressEl.hidden = true;
  indexProgressFill.style.width = "0";
}

// Краткая надпись в области прогресса (итог/ошибка), затем авто-скрытие.
function flashIndexLabel(text: string, isError: boolean) {
  indexProgressEl.hidden = false;
  indexProgressFill.style.width = "0";
  indexProgressLabel.textContent = text;
  indexProgressLabel.classList.toggle("danger", isError);
  setTimeout(() => {
    indexProgressLabel.classList.remove("danger");
    indexProgressEl.hidden = true;
  }, 3500);
}

// «Добавить документ»: выбор файла (плагин Фазы A) → индексация с прогрессом.
async function addDocument() {
  let path: string | null;
  try {
    const sel = await open({
      multiple: false,
      filters: [{ name: "Документы", extensions: ["pdf", "docx", "txt", "md"] }],
    });
    path = typeof sel === "string" ? sel : null;
  } catch (e) {
    flashIndexLabel(`Не удалось открыть диалог: ${e}`, true);
    return;
  }
  if (!path) return;

  addDocBtn.disabled = true;
  showIndexProgress("Чтение документа…", 0.04);

  const onProgress = new Channel<IndexProgress>();
  onProgress.onmessage = (p) => {
    const frac = p.total ? p.current / p.total : 0;
    if (p.phase === "chunk") showIndexProgress(`Подготовка фрагментов: ${p.total}`, 0.08);
    else if (p.phase === "embed") showIndexProgress(`Индексация: ${p.current} из ${p.total}`, frac);
    else if (p.phase === "done") showIndexProgress("Сохранение…", 1);
  };

  try {
    const res = await invoke<{ status: string; document: DocumentMeta; rebuilt: boolean }>(
      "index_document",
      { path, onProgress },
    );
    await refreshDocuments();
    if (res.rebuilt) {
      // сменилась модель эмбеддингов → база пересоздана под новую размерность
      flashIndexLabel("База пересоздана под новую модель поиска — прежние документы добавьте заново", true);
    } else if (res.status === "exists") {
      flashIndexLabel(`«${res.document.filename}» уже в базе`, false);
    } else {
      flashIndexLabel(`Добавлен: ${res.document.filename}`, false);
    }
  } catch (e) {
    hideIndexProgress();
    flashIndexLabel(String(e), true);
  } finally {
    addDocBtn.disabled = false;
  }
}

async function deleteDocument(d: DocumentMeta) {
  if (!(await confirmModal(`Удалить «${d.filename}» из базы документов?`))) return;
  try {
    await invoke("delete_document", { id: d.id });
  } catch (e) {
    flashIndexLabel(`Не удалось удалить: ${e}`, true);
    return;
  }
  await refreshDocuments();
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

// «Установить bge-m3»: тянет модель через Rust (pull_model) с прогрессом и отменой.
async function installEmbeddingModel() {
  installEmbedBtn.hidden = true;
  docStatusEl.hidden = true; // на месте карточки — прогресс
  pulling = true;
  pullCancelled = false;
  pullCancelBtn.hidden = false;
  pullCancelBtn.disabled = false;
  showIndexProgress("Подготовка установки…", 0.02);

  const onEvent = new Channel<PullEvent>();
  onEvent.onmessage = (e) => {
    if (e.type !== "progress") return;
    const frac = e.total > 0 ? e.completed / e.total : 0;
    const ru = ruPullStatus(e.status);
    const tail = e.total > 0 ? ` ${Math.round(frac * 100)}% (${gb(e.completed)} из ${gb(e.total)})` : "";
    showIndexProgress(`${ru}${tail}`, frac);
  };

  try {
    await invoke("pull_model", { name: "bge-m3", onEvent });
    pullCancelBtn.hidden = true;
    if (pullCancelled) {
      flashIndexLabel("Установка отменена — можно докачать позже (Ollama продолжит с места)", false);
    } else {
      flashIndexLabel("Модель bge-m3 установлена", false);
      // модель появилась — обновляем статус базы и список моделей без перезапуска
      await refreshDocuments();
      await loadModels();
    }
  } catch (e) {
    pullCancelBtn.hidden = true;
    flashIndexLabel(String(e), true);
  } finally {
    pulling = false;
    pullCancelBtn.hidden = true;
    // если модель так и не установилась — вернуть карточку с кнопкой
    if (!embeddingReady) {
      docStatusEl.hidden = false;
      installEmbedBtn.hidden = false;
      installEmbedBtn.disabled = false;
    }
  }
}

function cancelPull() {
  pullCancelled = true;
  pullCancelBtn.disabled = true;
  invoke("cancel_pull").catch(() => {});
  showIndexProgress("Отмена…", 0);
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
  feedEl.hidden = true;
  composerWrapEl.hidden = true;
  settingsView.hidden = false;
  settingsBtn.classList.add("active");
  refreshEnginePaths(); // подтянуть актуальные пути при открытии
}

// Вернуться назад: страница скрывается, лента и поле ввода возвращаются.
function closeSettings() {
  settingsView.hidden = true;
  feedEl.hidden = false;
  composerWrapEl.hidden = false;
  settingsBtn.classList.remove("active");
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
    await refreshDocuments();
    await loadModels();
    if (res.status === "external") report(res.message, false);
    else if (embeddingReady) report("Локальный каталог моделей применён", false);
    else report("Каталог применён, но bge-m3 в нём не найдена", true);
  } catch (e) {
    report(String(e), true); // напр. «не похоже на каталог моделей Ollama»
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
  showIndexProgress("Применение локального каталога…", 0.4);
  await applyModelsDir(dir, (t, err) => flashIndexLabel(t, err));
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
    await refreshDocuments();
    await loadModels();
    settingsStatus("Пути сброшены — авто-разрешение (ресурс → PATH).", false);
  } catch (e) {
    settingsStatus(String(e), true);
  }
  refreshEnginePaths();
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
  for (const m of history) addBubble(m.role, m.content, m.doc, m.sources, m.images);
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

// Рисует список диалогов с учётом поиска и групп по датам.
function renderConvList() {
  convListEl.innerHTML = "";
  const f = convFilter.trim().toLowerCase();
  const items = f
    ? convMetas.filter((m) => (m.title || "").toLowerCase().includes(f))
    : convMetas;

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

// Открывает диалог из файла в ленту.
async function openConversation(id: string) {
  if (streaming) stop();
  if (!settingsView.hidden) closeSettings(); // вышли из настроек — показываем ленту
  let conv: Conversation;
  try {
    conv = await invoke<Conversation>("load_conversation", { id });
  } catch (e) {
    addError(`Не удалось открыть диалог: ${e}`);
    return;
  }
  currentId = conv.id;
  history.length = 0;
  history.push(...conv.messages);
  renderHistory();
  refreshConversationList();
  inputEl.focus();
}

// «Новый диалог»: пустой чат. Старый уже сохранён — ничего не теряется.
function newDialog() {
  if (streaming) stop();
  if (!settingsView.hidden) closeSettings(); // вышли из настроек — показываем ленту
  currentId = crypto.randomUUID();
  history.length = 0;
  shownSourceFiles.clear(); // новый диалог — источники снова показываем с первого раза
  messagesEl.innerHTML = "";
  refreshEmptyState(); // пустой диалог → показываем приветствие
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
    newDialog();
  } else {
    refreshConversationList();
  }
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
    addError(`Не удалось получить список моделей: ${err}`);
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
  modelSelectEl.innerHTML = "";
  for (const m of models) {
    thinkingByModel.set(m.name, m.thinking);
    visionByModel.set(m.name, m.vision);
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
  vram_gb: number | null;
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
  if (hw.vram_gb != null) specs.push(`GPU${nb}${hw.vram_gb.toFixed(0)}${nb}ГБ`);
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
  indexProgressEl = document.querySelector("#index-progress")!;
  indexProgressFill = document.querySelector("#index-progress-fill")!;
  indexProgressLabel = document.querySelector("#index-progress-label")!;
  tabChatsBtn.addEventListener("click", () => switchTab("chats"));
  tabDocsBtn.addEventListener("click", () => switchTab("docs"));
  addDocBtn.addEventListener("click", addDocument);
  installEmbedBtn.addEventListener("click", installEmbeddingModel);
  installLocalBtn.addEventListener("click", installFromLocalDir);
  pullCancelBtn.addEventListener("click", cancelPull);
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
  newChatBtn.addEventListener("click", newDialog);
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
  await initConversations(); // сначала восстановим диалоги в ленту
  loadHardware();            // неблокирующе: светофор железа (локально, без движка)
  // Сначала обеспечиваем движок (поднимаем свой или переиспользуем системный), затем
  // уже опираемся на него. Если не готов — статус выставлен, движок-зависимые шаги
  // пропускаем (пользователь может повторить кнопкой «Проверка»).
  const engineReady = await ensureEngine();
  if (engineReady) {
    checkOllama();           // покажет версию Ollama в шапке
    loadModels();
    refreshDocuments();      // число документов (для RAG) и статус модели эмбеддингов
  }
});
