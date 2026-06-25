import { invoke, Channel } from "@tauri-apps/api/core";

// Модель по умолчанию. Выбор модели из набора — отдельный шаг (выпадающий список).
// Текущая выбранная модель (заполняется из списка установленных).
let selectedModel = "";

// Лёгкая системная подсказка — задаёт тон ассистента.
const SYSTEM = { role: "system" as const, content: "Ты — полезный ассистент. Отвечай по-русски." };

// События из Rust (см. ChatEvent в lib.rs).
type ChatEvent = { type: "chunk"; content: string } | { type: "done" };

type Role = "user" | "assistant";
interface Message {
  role: Role;
  content: string;
}

// История диалога (без системного сообщения — его добавляем при отправке).
const history: Message[] = [];

// Счётчик «поколений»: позволяет кнопке «Стоп» игнорировать поздние кусочки.
let generation = 0;
let streaming = false;

let messagesEl: HTMLElement;
let inputEl: HTMLTextAreaElement;
let sendBtn: HTMLButtonElement;
let stopBtn: HTMLButtonElement;
let modelSelectEl: HTMLSelectElement;
let statusEl: HTMLElement;
let refreshBtn: HTMLButtonElement;
let hwBarEl: HTMLElement;

// Создаёт пузырь сообщения и возвращает элемент с текстом (для дозаписи).
function addBubble(role: Role, text: string): HTMLElement {
  const row = document.createElement("div");
  row.className = `msg msg--${role}`;
  const body = document.createElement("div");
  body.className = "msg__body";
  body.textContent = text;
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

  const myGen = ++generation;
  setStreaming(true);

  // Пузырь ассистента, в который будем дописывать ответ.
  const answerEl = addBubble("assistant", "");
  let answer = "";
  let settled = false;

  const finish = () => {
    if (settled || myGen !== generation) return;
    settled = true;
    if (answer.trim()) {
      history.push({ role: "assistant", content: answer });
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
      answerEl.textContent = answer;
      scrollToBottom();
    } else if (msg.type === "done") {
      finish();
    }
  };

  try {
    await invoke("chat_stream", {
      model: selectedModel,
      messages: [SYSTEM, ...history],
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
  let hw: HardwareInfo;
  try {
    hw = await invoke<HardwareInfo>("detect_hardware");
  } catch {
    hwBarEl.textContent = "Конфигурация оборудования не определена";
    hwBarEl.className = "chat__hw";
    hwBarEl.hidden = false;
    return;
  }

  const emoji = hw.tier === "green" ? "🟢" : hw.tier === "yellow" ? "🟡" : "🔴";
  const rec =
    hw.tier === "green"
      ? "рекомендуемая модель: qwen3.5:9b"
      : hw.tier === "yellow"
        ? "рекомендуемая модель: qwen3.5:4b"
        : "рекомендуются модели до 4B";

  const parts = [`ОЗУ ${hw.ram_gb.toFixed(0)} ГБ`, `${hw.cpu_cores} ядер`];
  if (hw.vram_gb != null) {
    parts.push(`видеопамять ${hw.vram_gb.toFixed(0)} ГБ`);
  } else if (hw.vram_source === "unified") {
    parts.push("общая память");
  }

  hwBarEl.textContent = `${emoji} ${parts.join(" · ")} — ${rec}`;
  hwBarEl.className = `chat__hw chat__hw--${hw.tier}`;
  hwBarEl.hidden = false;
}

// Мягкая проверка движка: спрашиваем версию Ollama и показываем её в шапке.
// Неблокирующая — при недоступности просто показываем статус, приложение работает.
async function checkOllama() {
  try {
    const version = await invoke<string>("ollama_version");
    statusEl.textContent = `Ollama ${version}`;
    statusEl.classList.remove("chat__status--down");
  } catch {
    statusEl.textContent = "Ollama недоступна — запустите движок";
    statusEl.classList.add("chat__status--down");
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

// Авто-высота поля ввода под текст.
function autoGrow() {
  inputEl.style.height = "auto";
  inputEl.style.height = Math.min(inputEl.scrollHeight, 200) + "px";
}

window.addEventListener("DOMContentLoaded", () => {
  messagesEl = document.querySelector("#messages")!;
  inputEl = document.querySelector("#chat-input")!;
  sendBtn = document.querySelector("#send-btn")!;
  stopBtn = document.querySelector("#stop-btn")!;
  modelSelectEl = document.querySelector("#model-select")!;
  statusEl = document.querySelector("#status")!;
  refreshBtn = document.querySelector("#refresh-btn")!;
  hwBarEl = document.querySelector("#hw-bar")!;
  modelSelectEl.addEventListener("change", () => {
    selectedModel = modelSelectEl.value;
  });
  refreshBtn.addEventListener("click", refreshAll);

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
  checkOllama();             // неблокирующе: статус движка в шапке
  loadHardware();            // неблокирующе: светофор железа под шапкой
  loadModels();
});
