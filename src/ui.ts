// Базовые примитивы интерфейса, нужные многим модулям: пузыри ленты, уведомления,
// прокрутка, модальные окна, источники под ответом, «Стоп», переключение на экран
// чата. Зависит только от state/dom/util — модули фич импортируют его без циклов.

import { invoke } from "@tauri-apps/api/core";
import type { DocAttachment, Role, SourceRef, WebSource } from "./types";
import { state, shownSourceFiles } from "./state";
import {
  composerWrapEl,
  emptyStateEl,
  feedEl,
  gentleStateTextEl,
  gentleToggleBtn,
  inputEl,
  messagesEl,
  modelsStatusEl,
  projectListEl,
  projectView,
  sendBtn,
  settingsBtn,
  settingsView,
  stopBtn,
  wizardView,
} from "./dom";
import { docSubline, fileFormat, imageDataUrl } from "./util";

// ── Ленивый Markdown-рендер ───────────────────────────────────────────────────
// Тяжёлый рендер (markdown-it + KaTeX + highlight.js + их CSS) — отдельный чанк:
// НЕ грузится при старте, а подтягивается в фоне сразу после него. Если ответ
// пришёл раньше, чем чанк, — показываем простым текстом и «дорисовываем» по
// готовности (локально чанк загружается за миллисекунды, это подстраховка).
let renderMd: ((text: string) => string) | null = null;
const mdReady = import("./markdown").then(
  (m) => {
    renderMd = m.renderMarkdown;
  },
  (e) => {
    console.error("markdown-чанк не загрузился — ответы будут простым текстом:", e);
  },
);

// Отрисовать текст ассистента в элемент: Markdown, когда рендер готов; при сбое
// рендера — весь текст как есть (содержание ответа важнее форматирования).
export function renderMarkdownInto(el: HTMLElement, text: string): void {
  const paint = () => {
    if (!renderMd) return; // чанк так и не загрузился — остаётся простой текст
    try {
      el.innerHTML = renderMd(text);
    } catch {
      el.textContent = text; // форматирование упало — показываем хотя бы весь текст
    }
  };
  if (renderMd) {
    paint();
    return;
  }
  el.textContent = text;
  void mdReady.then(paint);
}

// ── Действия под ответом ассистента ──────────────────────────────────────────

// Обработчик «Повторить» регистрирует chat.ts (ui.ts не импортирует его — без циклов).
let retryHandler: (() => void) | null = null;
export function setRetryHandler(fn: () => void) {
  retryHandler = fn;
}

// «Повторить» имеет смысл только у ПОСЛЕДНЕГО ответа: при появлении любого
// следующего хода кнопки прежних ответов прячем.
function retireRetryButtons() {
  messagesEl
    .querySelectorAll<HTMLButtonElement>(".msg-act--retry")
    .forEach((b) => (b.hidden = true));
}

// Панель действий под ответом: «Копировать» — весь ответ как текст (Markdown-исходник,
// вставляется куда угодно), «Повторить» — перегенерировать ответ на тот же вопрос.
export function addTurnActions(turn: HTMLElement, text: string) {
  const row = document.createElement("div");
  row.className = "msg-actions";

  const copy = document.createElement("button");
  copy.type = "button";
  copy.className = "msg-act";
  copy.textContent = "Копировать";
  copy.addEventListener("click", () => {
    const restore = () => setTimeout(() => (copy.textContent = "Копировать"), 1500);
    navigator.clipboard.writeText(text).then(
      () => {
        copy.textContent = "✓ Скопировано";
        restore();
      },
      () => {
        copy.textContent = "Не удалось";
        restore();
      },
    );
  });
  row.appendChild(copy);

  const retry = document.createElement("button");
  retry.type = "button";
  retry.className = "msg-act msg-act--retry";
  retry.textContent = "Повторить";
  retry.title = "Сгенерировать ответ заново";
  retry.addEventListener("click", () => {
    if (state.streaming) return;
    retryHandler?.();
  });
  row.appendChild(retry);

  turn.appendChild(row);
}

// ── Пузыри и уведомления ленты ───────────────────────────────────────────────

// Карточка прикреплённого файла внутри пузыря сообщения (иконка-бейдж + имя + размер).
export function buildDocCard(doc: DocAttachment): HTMLElement {
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
export function addBubble(
  role: Role,
  text: string,
  doc?: DocAttachment,
  sources?: SourceRef[],
  images?: string[],
  webSources?: WebSource[],
): HTMLElement {
  retireRetryButtons(); // любой новый ход делает прежние «Повторить» неактуальными
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
    renderMarkdownInto(body, text); // ответ — как Markdown/формулы, без аватара/подписи
    turn.appendChild(body);
    if (sources && sources.length) renderSources(turn, sources); // источники из базы
    if (webSources && webSources.length) renderWebSources(turn, webSources); // из интернета
    addTurnActions(turn, text); // «Копировать»/«Повторить» и у ответов из истории
  }
  messagesEl.appendChild(turn);
  refreshEmptyState();
  scrollToBottom();
  return body;
}

// Строит ответ ассистента: индикатор «думаю» (точки), переливающееся
// «Рассуждение» с текстом (без рамки/коллапса) и контейнер ответа.
export function addAssistantTurn() {
  retireRetryButtons(); // начинается новый ответ — прежние «Повторить» неактуальны
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

export function addError(text: string) {
  const row = document.createElement("div");
  row.className = "err";
  row.textContent = text;
  messagesEl.appendChild(row);
  refreshEmptyState();
  scrollToBottom();
}

// Нейтральное уведомление в ленте (не ошибка) — напр. предупреждение об усечении.
export function addNotice(text: string) {
  const row = document.createElement("div");
  row.className = "notice";
  row.textContent = text;
  messagesEl.appendChild(row);
  refreshEmptyState();
  scrollToBottom();
}

export function scrollToBottom() {
  if (!state.autoScroll) return; // прокрутил вверх — не тянем обратно вниз
  feedEl.scrollTop = feedEl.scrollHeight;
}

// Приветствие видно, только когда в открытом диалоге нет сообщений.
export function refreshEmptyState() {
  emptyStateEl.hidden = messagesEl.children.length > 0;
}

// ── Состояние композера и «Стоп» ─────────────────────────────────────────────

export function setStreaming(on: boolean) {
  state.streaming = on;
  sendBtn.hidden = on;
  stopBtn.hidden = !on;
  inputEl.disabled = on;
  if (!on && state.selectedModel) inputEl.focus(); // вернуть фокус в поле после ответа
}

export function setComposerEnabled(on: boolean) {
  inputEl.disabled = !on;
  sendBtn.disabled = !on;
}

// Подпись щадящего режима в настройках: включён/выключен (+пометка, если решение
// принял светофор железа, а не пользователь). Живёт здесь, а не в settings.ts,
// чтобы models.ts (авто-режим по железу) не тянул settings — без циклов импортов.
export function updateGentleUi() {
  gentleStateTextEl.textContent = state.gentleMode
    ? `включён — половина ядер, без перегрева${state.gentleAuto ? " (по оценке железа)" : ""}`
    : "выключен — полная скорость";
  gentleToggleBtn.textContent = state.gentleMode ? "Выключить" : "Включить";
}

export function stop() {
  if (!state.streaming) return;
  state.generation++; // «отвязываем» текущий запрос — поздние кусочки игнорируются
  invoke("cancel_stream").catch(() => {}); // и реально останавливаем генерацию в Ollama
  // Дочистить UI и сохранить частичный ответ (иначе «Думаю…» зависает, ответ теряется).
  if (state.activeStopCleanup) {
    const cleanup = state.activeStopCleanup;
    state.activeStopCleanup = null;
    cleanup();
  }
  setStreaming(false);
}

// Показать экран чата, спрятав настройки и экран проекта (идемпотентно). Общая
// точка для conversations/settings — без импорта их модулей (разрыв циклов).
export function showChatView() {
  settingsView.hidden = true;
  settingsBtn.classList.remove("active");
  modelsStatusEl.hidden = true;
  projectView.hidden = true;
  wizardView.hidden = true;
  state.viewingProjectId = null;
  projectListEl.querySelectorAll(".project.active").forEach((el) => el.classList.remove("active"));
  feedEl.hidden = false;
  composerWrapEl.hidden = false;
}

// ── Источники под ответом ────────────────────────────────────────────────────

// Рисует строку источников под ответом — но КАЖДЫЙ документ упоминаем один раз за
// диалог. Если все источники этого ответа уже показывались ранее — заметку не рисуем
// (не повторяем под каждым сообщением). Новый документ, попавший в дело, покажем один раз.
export function renderSources(turn: HTMLElement, sources: SourceRef[]) {
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
export function renderWebSources(turn: HTMLElement, items: WebSource[]) {
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

// ── Модальные окна (нативные confirm()/prompt() в Tauri-окне не работают) ─────

// Своё модальное подтверждение.
export function confirmModal(message: string, okLabel = "Удалить"): Promise<boolean> {
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

// Модальный ввод строки. Возвращает введённую строку или null при отмене.
export function promptModal(title: string, placeholder = ""): Promise<string | null> {
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
