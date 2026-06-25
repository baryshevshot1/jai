import { invoke, Channel } from "@tauri-apps/api/core";
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

// События из Rust (см. ChatEvent в lib.rs).
type ChatEvent = { type: "chunk"; content: string } | { type: "done" };

type Role = "user" | "assistant";
interface Message {
  role: Role;
  content: string;
}

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

// Режим рассуждений (тумблер). По умолчанию включён; выбор хранится в localStorage.
let thinkEnabled = true;

let messagesEl: HTMLElement;
let inputEl: HTMLTextAreaElement;
let sendBtn: HTMLButtonElement;
let stopBtn: HTMLButtonElement;
let modelSelectEl: HTMLSelectElement;
let statusEl: HTMLElement;
let refreshBtn: HTMLButtonElement;
let hwBarEl: HTMLElement;
let convListEl: HTMLElement;
let newChatBtn: HTMLButtonElement;
let thinkToggleEl: HTMLInputElement;
let themeBtn: HTMLButtonElement;
let convSearchEl: HTMLInputElement;
let checkBtn: HTMLButtonElement;
let settingsBtn: HTMLButtonElement;

// Заполняет пузырь: ответ ассистента — как Markdown/формулы, реплику
// пользователя — простым текстом (безопаснее, без неожиданной разметки).
function setBubble(body: HTMLElement, role: Role, text: string) {
  if (role === "assistant") {
    body.innerHTML = renderMarkdown(text);
  } else {
    body.textContent = text;
  }
}

// Создаёт пузырь сообщения и возвращает элемент с текстом (для дозаписи).
function addBubble(role: Role, text: string): HTMLElement {
  const row = document.createElement("div");
  row.className = `msg msg--${role}`;
  const body = document.createElement("div");
  body.className = "msg__body";
  setBubble(body, role, text);
  row.appendChild(body);
  messagesEl.appendChild(row);
  scrollToBottom();
  return body;
}

function addError(text: string) {
  const row = document.createElement("div");
  row.className = "msg msg--error";
  row.textContent = text;
  messagesEl.appendChild(row);
  scrollToBottom();
}

function scrollToBottom() {
  messagesEl.scrollTop = messagesEl.scrollHeight;
}

function setStreaming(on: boolean) {
  streaming = on;
  sendBtn.hidden = on;
  stopBtn.hidden = !on;
  inputEl.disabled = on;
}

async function send() {
  const text = inputEl.value.trim();
  if (!text || streaming || !selectedModel) return;

  inputEl.value = "";
  autoGrow();
  history.push({ role: "user", content: text });
  addBubble("user", text);
  persist(); // вопрос сохраняется сразу

  const myGen = ++generation;
  setStreaming(true);

  // Пузырь ассистента, в который будем дописывать ответ.
  const answerEl = addBubble("assistant", "");
  answerEl.textContent = "Думает…"; // пока нет текста ответа (особенно в режиме рассуждений)
  answerEl.classList.add("msg__pending");
  let answer = "";
  let settled = false;

  const finish = () => {
    if (settled || myGen !== generation) return;
    settled = true;
    if (answer.trim()) {
      history.push({ role: "assistant", content: answer });
      persist(); // сохраняем готовый ответ
    } else {
      answerEl.parentElement?.remove(); // пустой ответ — убираем пузырь
    }
    setStreaming(false);
  };

  const onEvent = new Channel<ChatEvent>();
  onEvent.onmessage = (msg) => {
    if (myGen !== generation) return; // нажали «Стоп» — игнорируем хвост
    if (msg.type === "chunk") {
      answer += msg.content;
      if (answer) {
        answerEl.classList.remove("msg__pending");
        answerEl.innerHTML = renderMarkdown(answer);
      }
      scrollToBottom();
    } else if (msg.type === "done") {
      finish();
    }
  };

  try {
    await invoke("chat_stream", {
      model: selectedModel,
      messages: [SYSTEM, ...history],
      think: thinkEnabled,
      onEvent,
    });
    finish(); // если поток закончился без явного "done"
  } catch (err) {
    if (settled || myGen !== generation) return;
    settled = true;
    if (!answer) answerEl.parentElement?.remove();
    addError(String(err));
    setStreaming(false);
  }
}

function stop() {
  if (!streaming) return;
  generation++; // «отвязываем» текущий запрос — поздние кусочки игнорируются
  setStreaming(false);
}

function setComposerEnabled(on: boolean) {
  inputEl.disabled = !on;
  sendBtn.disabled = !on;
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
  for (const m of history) addBubble(m.role, m.content);
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
  refreshConversationList(); // снимет подсветку (нового ещё нет в списке)
  inputEl.focus();
}

// Удаление диалога (с подтверждением — потеря данных необратима).
async function deleteConversation(id: string) {
  if (!confirm("Удалить этот диалог? Действие необратимо.")) return;
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
  } else {
    currentId = crypto.randomUUID(); // пустой новый диалог
    await refreshConversationList();
  }
}

// Тянет список установленных моделей из Ollama (через Rust-команду list_models)
// и заполняет выпадающий список в шапке.
async function loadModels() {
  let models: string[];
  try {
    models = await invoke<string[]>("list_models");
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

  modelSelectEl.innerHTML = "";
  for (const name of models) {
    const opt = document.createElement("option");
    opt.value = name;
    opt.textContent = name;
    modelSelectEl.appendChild(opt);
  }
  // Предпочитаем целевую базовую модель, если она установлена; иначе — первую.
  const preferred = "qwen3.5:9b";
  selectedModel = models.includes(preferred) ? preferred : models[0];
  modelSelectEl.value = selectedModel;
  modelSelectEl.disabled = false;
  setComposerEnabled(true);
  inputEl.focus();
}

// Сведения о железе (из Rust-команды detect_hardware).
interface HardwareInfo {
  ram_gb: number;
  cpu_cores: number;
  vram_gb: number | null;
  vram_source: string;
  tier: "green" | "yellow" | "red";
}

// «Светофор» железа: определяем ресурсы и показываем полоску под шапкой
// с рекомендацией модели. Неблокирующая — при сбое показываем нейтральный текст.
async function loadHardware() {
  const textEl = document.querySelector("#hw-text")!;
  let hw: HardwareInfo;
  try {
    hw = await invoke<HardwareInfo>("detect_hardware");
  } catch {
    textEl.textContent = "Железо: не определено";
    hwBarEl.className = "hwchip hwchip--unknown";
    hwBarEl.removeAttribute("title");
    hwBarEl.hidden = false;
    return;
  }

  const tierWord =
    hw.tier === "green" ? "зелёный" : hw.tier === "yellow" ? "жёлтый" : "красный";
  const rec =
    hw.tier === "green"
      ? "Рекомендуемая модель: qwen3.5:9b"
      : hw.tier === "yellow"
        ? "Рекомендуемая модель: qwen3.5:4b"
        : "Рекомендуются модели до 4B";

  const parts = [`${hw.ram_gb.toFixed(0)} ГБ`, `${hw.cpu_cores} ядер`];
  if (hw.vram_gb != null) parts.push(`${hw.vram_gb.toFixed(0)} ГБ VRAM`);

  textEl.innerHTML = `Железо: <b>${tierWord}</b> · ${parts.join(" · ")}`;
  hwBarEl.className = `hwchip hwchip--${hw.tier}`;
  hwBarEl.title = rec;
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
  await checkOllama();
  await loadModels();
  refreshBtn.disabled = false;
}

// «Проверка»: полная перепроверка движка, железа и списка моделей.
async function recheck() {
  checkBtn.disabled = true;
  await checkOllama();
  await Promise.all([loadHardware(), loadModels()]);
  checkBtn.disabled = false;
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
  settingsBtn = document.querySelector("#settings-btn")!;
  modelSelectEl.addEventListener("change", () => {
    selectedModel = modelSelectEl.value;
  });
  refreshBtn.addEventListener("click", refreshAll);
  newChatBtn.addEventListener("click", newDialog);
  themeBtn.addEventListener("click", toggleTheme);
  checkBtn.addEventListener("click", recheck);
  settingsBtn.addEventListener("click", () => {
    alert("Раздел «Настройки» появится в следующем этапе.");
  });
  convSearchEl.addEventListener("input", () => {
    convFilter = convSearchEl.value;
    renderConvList();
  });
  initTheme(); // применяем сохранённую/системную тему как можно раньше

  // Восстанавливаем тумблер «Размышления» (по умолчанию включён).
  const savedThink = localStorage.getItem("jai.think");
  thinkEnabled = savedThink === null ? true : savedThink === "true";
  thinkToggleEl.checked = thinkEnabled;
  thinkToggleEl.addEventListener("change", () => {
    thinkEnabled = thinkToggleEl.checked;
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
});
