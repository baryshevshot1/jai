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
// следующего хода кнопки прежних ответов прячем. Считаем обе разновидности —
// и под готовым ответом, и в строке ошибки: иначе кнопка у старой ошибки
// осталась бы на экране живой, а по клику молча ничего не делала.
export function retireRetryButtons() {
  messagesEl
    .querySelectorAll<HTMLButtonElement>(".msg-act--retry, .err-retry")
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

// ── Строка состояния операции ────────────────────────────────────────────────

// Один компонент на все карточки: итог операции всегда выглядит и ведёт себя
// одинаково, где бы он ни показывался (модели, движок, документы, обновления).
// Таймеры автоскрытия храним снаружи элементов — WeakMap не мешает сборке мусора
// и не заводит на DOM-узлах самодельных полей.
const statusTimers = new WeakMap<HTMLElement, number>();

// Успех сам гаснет: его прочитали — и он больше не нужен. «К сведению» и ошибка
// висят, пока их не сменит следующий итог: сообщение о сбое должно дождаться,
// когда пользователь его прочтёт, а не исчезнуть через несколько секунд.
const STATUS_SUCCESS_MS = 3500;

export type StatusKind = "info" | "success" | "error";

export function showStatus(el: HTMLElement, text: string, kind: StatusKind = "info"): void {
  clearStatusTimer(el);
  // Базовый класс добавляем, а чужие не трогаем: элемент может нести и свою вёрстку.
  el.classList.add("settings-status");
  el.classList.toggle("settings-status--success", kind === "success");
  el.classList.toggle("settings-status--error", kind === "error");
  el.textContent = text;
  el.hidden = false;
  // Ошибку экранный диктор объявляет сразу, остальное — не перебивая пользователя.
  el.setAttribute("role", kind === "error" ? "alert" : "status");
  if (kind === "success") {
    el.setAttribute("aria-live", "polite");
    statusTimers.set(
      el,
      window.setTimeout(() => {
        statusTimers.delete(el);
        el.hidden = true;
      }, STATUS_SUCCESS_MS),
    );
  } else {
    el.setAttribute("aria-live", kind === "error" ? "assertive" : "polite");
  }
}

export function hideStatus(el: HTMLElement): void {
  clearStatusTimer(el);
  el.hidden = true;
}

// Отложенное скрытие прежнего сообщения не должно погасить новое: любой повторный
// вызов начинает отсчёт заново.
function clearStatusTimer(el: HTMLElement) {
  const t = statusTimers.get(el);
  if (t !== undefined) {
    clearTimeout(t);
    statusTimers.delete(el);
  }
}

// ── Шаги первого запуска ─────────────────────────────────────────────────────

// Холодный старт занимает секунды: движок поднимается, модель заезжает в память.
// Без картинки происходящего это выглядит как «зависло», поэтому на приветственном
// экране показываем ход дела шагами — и сорванный шаг честно называем.
const BOOT_ORDER = ["engine", "models", "ready"] as const;
export type BootStep = (typeof BOOT_ORDER)[number] | "off";

// Куда идти, если шаг не удался: подсказка про конкретный шаг, а не общее «ошибка».
const BOOT_FAIL_HINT: Record<(typeof BOOT_ORDER)[number], string> = {
  engine: "Не удалось запустить движок. Откройте Настройки → «Мастер установки».",
  models: "Не удалось получить список моделей. Откройте Настройки → «Модели».",
  ready: "Приложение запустилось не полностью. Откройте Настройки → «Проверка системы».",
};

export function setBootStep(step: BootStep, failed = false): void {
  const box = document.getElementById("boot-steps");
  if (!box) return;
  if (step === "off") {
    box.hidden = true;
    return;
  }
  box.hidden = false;
  const at = BOOT_ORDER.indexOf(step);
  BOOT_ORDER.forEach((name, i) => {
    const row = box.querySelector<HTMLElement>(`.boot-step[data-step="${name}"]`);
    if (!row) return;
    // Последний шаг («Готов к работе») сам себе итог — отмечаем его зелёным сразу.
    const done = i < at || (i === at && step === "ready" && !failed);
    row.classList.toggle("is-done", done);
    row.classList.toggle("is-active", i === at && !failed && !done);
    row.classList.toggle("is-failed", i === at && failed);
  });

  let hint = box.querySelector<HTMLElement>(".boot-steps__hint");
  if (failed) {
    if (!hint) {
      hint = document.createElement("div");
      hint.className = "boot-steps__hint";
      box.appendChild(hint);
    }
    hint.textContent = BOOT_FAIL_HINT[step];
    hint.hidden = false;
  } else if (hint) {
    hint.hidden = true;
  }
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

// ── Переключение экранов ─────────────────────────────────────────────────────
// Место ленты диалогов занимают ещё три полноэкранные страницы: настройки, экран
// проекта и мастер установки. Раньше каждая при открытии сама гасила все остальные
// вручную — четыре набора флагов hidden, разбросанных по четырём модулям. Забыть
// один флаг означало показать два экрана сразу или ни одного, и добавление пятого
// экрана требовало правки в каждом из мест.
//
// Здесь видимость решается ОДНОЙ функцией: она знает про все экраны сразу, поэтому
// «забыть погасить» технически невозможно. Побочные действия открытия (обновить
// пути движка, перечитать документы и т. п.) остаются у самих модулей — роутер
// отвечает только за то, что видно на экране.
export type Screen = "chat" | "settings" | "project" | "wizard";

let currentScreen: Screen = "chat";

export function activeScreen(): Screen {
  return currentScreen;
}

export function showScreen(next: Screen) {
  currentScreen = next;

  // Лента и поле ввода — только на экране чата; остальные страницы занимают их место.
  const chat = next === "chat";
  feedEl.hidden = !chat;
  composerWrapEl.hidden = !chat;

  settingsView.hidden = next !== "settings";
  projectView.hidden = next !== "project";
  wizardView.hidden = next !== "wizard";

  // Кнопка настроек подсвечена ровно тогда, когда настройки открыты.
  settingsBtn.classList.toggle("active", next === "settings");
  // Итог операции из настроек не должен «переезжать» на другой экран.
  if (next !== "settings") modelsStatusEl.hidden = true;

  // Уходим с экрана проекта — снимаем и выбор, и подсветку строки в панели.
  // Класс снимаем напрямую: renderProjectList живёт в projects.ts, который сам
  // импортирует ui.ts, и обратный импорт замкнул бы цикл.
  if (next !== "project") {
    state.viewingProjectId = null;
    projectListEl
      .querySelectorAll(".project.active")
      .forEach((el) => el.classList.remove("active"));
  }
}

// Показать экран чата (идемпотентно). Общая точка для conversations/settings —
// без импорта их модулей (разрыв циклов).
export function showChatView() {
  showScreen("chat");
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

// Ловушка фокуса модального окна: Tab/Shift+Tab ходят по элементам окна по кругу,
// не убегая на фон (стандарт доступности диалогов, aria-modal сам фокус не держит).
function trapTab(e: KeyboardEvent, overlay: HTMLElement) {
  if (e.key !== "Tab") return;
  const items = [...overlay.querySelectorAll<HTMLElement>("button, input")];
  if (!items.length) return;
  const first = items[0];
  const last = items[items.length - 1];
  const active = document.activeElement;
  const inside = active instanceof HTMLElement && overlay.contains(active);
  if (e.shiftKey && (active === first || !inside)) {
    e.preventDefault();
    last.focus();
  } else if (!e.shiftKey && (active === last || !inside)) {
    e.preventDefault();
    first.focus();
  }
}

// Своё модальное подтверждение. Все вызовы — деструктивные («Удалить…»), поэтому
// подтверждение только адресным действием: клик или Enter/Space на СФОКУСИРОВАННОЙ
// кнопке (нативная активация). Глобального «Enter = подтвердить» нет — он срабатывал
// бы и при фокусе на «Отмене», превращая отказ в необратимое удаление.
export function confirmModal(message: string, okLabel = "Удалить"): Promise<boolean> {
  return new Promise((resolve) => {
    const opener = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    const overlay = document.createElement("div");
    overlay.className = "modal-overlay";
    overlay.innerHTML = `
      <div class="modal" role="dialog" aria-modal="true">
        <div class="modal__text"></div>
        <div class="modal__actions">
          <button type="button" class="modal__btn" data-act="cancel">Отмена</button>
          <button type="button" class="modal__btn modal__btn--danger" data-act="ok"></button>
        </div>
      </div>`;
    overlay.querySelector(".modal__text")!.textContent = message;
    overlay.querySelector('[data-act="ok"]')!.textContent = okLabel;
    // Имя диалога для скринридера — сам вопрос (id-ссылки не нужны, окно одно).
    overlay.querySelector(".modal")!.setAttribute("aria-label", message);
    document.body.appendChild(overlay);

    const close = (result: boolean) => {
      overlay.remove();
      document.removeEventListener("keydown", onKey);
      opener?.focus(); // фокус возвращается туда, откуда открыли (доступность)
      resolve(result);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") close(false);
      else trapTab(e, overlay);
    };
    overlay.addEventListener("click", (e) => {
      const t = e.target as HTMLElement;
      if (t === overlay || t.dataset.act === "cancel") close(false);
      else if (t.dataset.act === "ok") close(true);
    });
    document.addEventListener("keydown", onKey);
    // Стартовый фокус — на безопасной «Отмене»: случайный Enter/Space сразу после
    // открытия не должен стирать данные; удаление требует осознанного шага.
    (overlay.querySelector('[data-act="cancel"]') as HTMLButtonElement).focus();
  });
}

// Модальный ввод строки. Возвращает введённую строку или null при отмене.
export function promptModal(title: string, placeholder = ""): Promise<string | null> {
  return new Promise((resolve) => {
    const opener = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    const overlay = document.createElement("div");
    overlay.className = "modal-overlay";
    overlay.innerHTML = `
      <div class="modal" role="dialog" aria-modal="true">
        <div class="modal__text"></div>
        <input class="modal__input" type="text" />
        <div class="modal__actions">
          <button type="button" class="modal__btn" data-act="cancel">Отмена</button>
          <button type="button" class="modal__btn modal__btn--primary" data-act="ok">Создать</button>
        </div>
      </div>`;
    overlay.querySelector(".modal__text")!.textContent = title;
    overlay.querySelector(".modal")!.setAttribute("aria-label", title); // имя диалога для скринридера
    const input = overlay.querySelector(".modal__input") as HTMLInputElement;
    input.placeholder = placeholder;
    document.body.appendChild(overlay);

    const close = (result: string | null) => {
      overlay.remove();
      document.removeEventListener("keydown", onKey);
      opener?.focus(); // фокус возвращается туда, откуда открыли (доступность)
      resolve(result);
    };
    const onKey = (e: KeyboardEvent) => {
      // Enter подтверждает только из поля ввода (привычно для формы); на кнопках
      // Enter/Space активируют СФОКУСИРОВАННУЮ кнопку нативно — «Отмена» отменяет.
      if (e.key === "Escape") close(null);
      else if (e.key === "Enter" && document.activeElement === input) close(input.value);
      else trapTab(e, overlay);
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
