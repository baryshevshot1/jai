import { invoke, Channel } from "@tauri-apps/api/core";

// Модель по умолчанию. Выбор модели из набора — отдельный шаг (выпадающий список).
const MODEL = "qwen3.5:9b";

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
  if (!text || streaming) return;

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
      model: MODEL,
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
  document.querySelector("#model-label")!.textContent = MODEL;

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
  inputEl.focus();
});
