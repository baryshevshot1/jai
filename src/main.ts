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

// Число документов в базе: >0 → перед ответом ищем релевантные фрагменты.
let docsCount = 0;
// Установлена ли модель эмбеддингов (без неё индексация/поиск невозможны).
let embeddingReady = false;

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
}

// Модель + поддержка рассуждений (из list_models).
interface ModelInfo {
  name: string;
  thinking: boolean;
}
// Какие модели поддерживают «Размышления» (имя → bool).
const thinkingByModel = new Map<string, boolean>();

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
let convSearchEl: HTMLInputElement;
let checkBtn: HTMLButtonElement;
let clearBtn: HTMLButtonElement;
let attachBtn: HTMLButtonElement;
let docChipEl: HTMLElement;
let docChipBadgeEl: HTMLElement;
let docChipNameEl: HTMLElement;
let docRemoveBtn: HTMLButtonElement;
let tabChatsBtn: HTMLButtonElement;
let tabDocsBtn: HTMLButtonElement;
let paneChatsEl: HTMLElement;
let paneDocsEl: HTMLElement;
let addDocBtn: HTMLButtonElement;
let docListEl: HTMLElement;
let docStatusEl: HTMLElement;
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
): HTMLElement {
  const turn = document.createElement("div");
  let body: HTMLElement;
  if (role === "user") {
    turn.className = "turn me";
    if (doc) turn.appendChild(buildDocCard(doc)); // карточка файла — над текстом запроса
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
function buildApiMessages(contextMsg: Message | null): { role: string; content: string }[] {
  const out: { role: string; content: string }[] = [SYSTEM];
  const msgs = contextMsg ? history.slice(-RAG_HISTORY_LIMIT) : history;
  for (let i = 0; i < msgs.length; i++) {
    const m = msgs[i];
    if (contextMsg && i === msgs.length - 1 && m.role === "user") {
      out.push({ role: contextMsg.role, content: contextMsg.content });
    }
    if (m.role === "user" && m.doc) {
      out.push({
        role: "user",
        content:
          `К сообщению прикреплён документ «${m.doc.name}». Его содержимое:\n\n` +
          `${m.doc.text}\n\n` +
          `— Опираясь на этот документ, ответь на вопрос: ${m.content}`,
      });
    } else {
      out.push({ role: m.role, content: m.content });
    }
  }
  return out;
}

async function send() {
  const text = inputEl.value.trim();
  if (!text || streaming || !selectedModel) return;

  inputEl.value = "";
  autoGrow();
  // Документ из композера привязываем к ЭТОМУ сообщению и сразу убираем из поля
  // ввода — он «уехал» вместе с вопросом и больше не висит над строкой.
  const doc = pendingDoc ?? undefined;
  clearPendingDoc();
  history.push({ role: "user", content: text, ...(doc ? { doc } : {}) });
  addBubble("user", text, doc);
  persist(); // вопрос (с файлом) сохраняется сразу
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
  // подсказка про отсутствие модели эмбеддингов (без неё база не работает)
  if (embeddingReady) {
    docStatusEl.hidden = true;
  } else {
    docStatusEl.hidden = false;
    docStatusEl.textContent =
      "Модель поиска по документам не установлена. Установите её командой: ollama pull bge-m3";
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

// Рисует строку источников под ответом ассистента (сгруппировано по документу).
function renderSources(turn: HTMLElement, sources: SourceRef[]) {
  if (!sources.length) return;
  const byDoc = new Map<string, number[]>();
  for (const s of sources) {
    const arr = byDoc.get(s.filename) ?? [];
    arr.push(s.chunk_index + 1);
    byDoc.set(s.filename, arr);
  }
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
  autoScroll = true; // открыли диалог — показываем низ (последние сообщения)
  for (const m of history) addBubble(m.role, m.content, m.doc, m.sources);
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
  currentId = crypto.randomUUID();
  history.length = 0;
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
  modelSelectEl.innerHTML = "";
  for (const m of models) {
    thinkingByModel.set(m.name, m.thinking);
    const opt = document.createElement("option");
    opt.value = m.name;
    opt.textContent = m.name;
    modelSelectEl.appendChild(opt);
  }
  // Предпочитаем целевую базовую модель, если она установлена; иначе — первую.
  const names = models.map((m) => m.name);
  // Сохраняем текущий выбор пользователя при «Обновить»; иначе целевая, иначе первая.
  if (!selectedModel || !names.includes(selectedModel)) {
    const preferred = "qwen3.5:9b";
    selectedModel = names.includes(preferred) ? preferred : names[0];
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
  const textEl = document.querySelector("#hw-text")!;
  let hw: HardwareInfo;
  try {
    hw = await invoke<HardwareInfo>("detect_hardware");
  } catch {
    textEl.textContent = "Конфигурация не определена";
    hwBarEl.className = "hwchip hwchip--unknown";
    hwBarEl.removeAttribute("title");
    hwBarEl.hidden = false;
    return;
  }

  // Цвет — у кружка (классы hwchip--*), в тексте — словесная оценка.
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

  textEl.innerHTML = `<b>${word}</b> · ${specs.join(" · ")}`;
  hwBarEl.className = `hwchip hwchip--${hw.tier}`;
  hwBarEl.title = `Рекомендуется ${model}`;
  hwBarEl.hidden = false;
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
  convSearchEl = document.querySelector("#conv-search")!;
  checkBtn = document.querySelector("#check-btn")!;
  clearBtn = document.querySelector("#clear-btn")!;
  attachBtn = document.querySelector("#attach-btn")!;
  docChipEl = document.querySelector("#doc-chip")!;
  docChipBadgeEl = document.querySelector("#doc-chip-badge")!;
  docChipNameEl = document.querySelector("#doc-chip-name")!;
  docRemoveBtn = document.querySelector("#doc-remove")!;
  attachBtn.addEventListener("click", attachDocument);
  docRemoveBtn.addEventListener("click", removeDocument);
  tabChatsBtn = document.querySelector("#tab-chats-btn")!;
  tabDocsBtn = document.querySelector("#tab-docs-btn")!;
  paneChatsEl = document.querySelector("#pane-chats")!;
  paneDocsEl = document.querySelector("#pane-docs")!;
  addDocBtn = document.querySelector("#add-doc-btn")!;
  docListEl = document.querySelector("#doc-list")!;
  docStatusEl = document.querySelector("#doc-status")!;
  indexProgressEl = document.querySelector("#index-progress")!;
  indexProgressFill = document.querySelector("#index-progress-fill")!;
  indexProgressLabel = document.querySelector("#index-progress-label")!;
  tabChatsBtn.addEventListener("click", () => switchTab("chats"));
  tabDocsBtn.addEventListener("click", () => switchTab("docs"));
  addDocBtn.addEventListener("click", addDocument);
  modelSelectEl.addEventListener("change", () => {
    selectedModel = modelSelectEl.value;
    updateThinkAvailability(); // у новой модели могут быть другие возможности
  });
  refreshBtn.addEventListener("click", refreshAll);
  newChatBtn.addEventListener("click", newDialog);
  themeBtn.addEventListener("click", toggleTheme);
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

  // Восстанавливаем тумблер «Размышления» (по умолчанию ВЫКЛ: с reasoning-моделью
  // даже простые вопросы думаются по ~20 секунд — для ассистента это непрактично).
  const savedThink = localStorage.getItem("jai.think");
  thinkEnabled = savedThink === null ? false : savedThink === "true";
  thinkToggleEl.classList.toggle("on", thinkEnabled);
  thinkToggleEl.addEventListener("click", () => {
    thinkEnabled = !thinkEnabled;
    thinkToggleEl.classList.toggle("on", thinkEnabled);
    localStorage.setItem("jai.think", String(thinkEnabled));
  });

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
  checkOllama();             // неблокирующе: статус движка в шапке
  loadHardware();            // неблокирующе: светофор железа под шапкой
  loadModels();
  refreshDocuments();        // неблокирующе: число документов (для RAG) и статус модели
});
