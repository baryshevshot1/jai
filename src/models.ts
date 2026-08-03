// Модели и движок: АВТО-подбор модели под задачу и железо (ручной выбор — в настройках),
// каталог набора в настройках (состояния, установка/обновление, проверка обновлений),
// «светофор» железа, обеспечение/проверка движка Ollama.

import { invoke } from "@tauri-apps/api/core";
import type { HardwareInfo, ModelInfo, ModelState } from "./types";
import {
  autoTagsForRole,
  modelSet,
  state,
  thinkingByModel,
  toolsByModel,
  updateByTag,
  visionByModel,
} from "./state";
import {
  checkUpdatesBtn,
  engineEl,
  engineMetaEl,
  engineVersionEl,
  hwBarEl,
  hwModelEl,
  hwModelNameEl,
  hwSpecsEl,
  hwWordEl,
  inputEl,
  modelChoiceEl,
  modelChoiceStateEl,
  modelListEl,
  modelProgressEl,
  modelProgressFill,
  modelProgressLabel,
  modelPullCancelBtn,
  modelsStatusEl,
  refreshBtn,
  statusEl,
  thinkToggleEl,
} from "./dom";
import {
  gb,
  humanError,
  ICON_ALERT,
  ICON_CHECK,
  ICON_DOWNLOAD,
  ICON_REFRESH_CW,
  plural,
} from "./util";
import {
  addError,
  setComposerEnabled,
  setToggleState,
  showStatus,
  updateGentleUi,
  type StatusKind,
} from "./ui";
import { cancelActivePull, runPull } from "./pull";
import { refreshDocuments, sidebarDocCtx } from "./documents";

let modelStates: ModelState[] = [];

// Установленные модели, умеющие отвечать (эмбеддинги bge-m3 отсеивает Rust).
let installed: ModelInfo[] = [];
// Список не удалось получить (движок не отвечает) — это НЕ то же самое, что
// «моделей нет»: в подписи к выбору модели говорим об этом честно.
let listFailed = false;

// ── Человеческие имена моделей ───────────────────────────────────────────────
// «qwen3.5:9b» заказчику ни о чём не говорит — по делу модель называется
// «Базовая текстовая (чат)». Имена берём из набора в Rust (model_states), своего
// второго списка не заводим: разойдутся — пользователь увидит разное в разных местах.
const titleByTag = new Map<string, string>();

// Запомнить набор: имена для показа и роль/вес/участие в авто-подборе (последнее —
// в общем modelSet, его читают и другие модули). true — имена изменились и подписи
// стоит перерисовать.
function rememberTitles(list: ModelState[]): boolean {
  let changed = false;
  for (const m of list) {
    modelSet.set(m.tag, { role: m.role, approxGb: m.approx_gb, autoPick: m.auto_pick });
    if (titleByTag.get(m.tag) !== m.title) {
      titleByTag.set(m.tag, m.title);
      changed = true;
    }
  }
  return changed;
}

// Спросить имена один раз при первой загрузке списка моделей: строка активной
// модели видна на экране чата сразу, а страницу настроек (где имена приходят
// вместе со списком набора) пользователь может не открыть вовсе.
let titlesAsked = false;
async function ensureModelTitles(): Promise<void> {
  if (titlesAsked || titleByTag.size) return;
  titlesAsked = true;
  try {
    const res = await invoke<{ models: ModelState[] }>("model_states");
    rememberTitles(res.models);
  } catch {
    titlesAsked = false; // движок ещё не отвечает — спросим при следующей загрузке
  }
}

// Имя модели для человека. Модель не из набора (пользователь поставил свою) —
// имени у неё нет, тогда честно показываем тег: он и есть её единственное имя.
function humanModel(tag: string): string {
  return titleByTag.get(tag) ?? tag;
}

// ── Авто-выбор модели ────────────────────────────────────────────────────────
// Модель в шапке больше НЕ выбирают: приложение подбирает её само.
//   • роль — по задаче: вопрос с картинкой → зрение, обычный вопрос → текст;
//   • вес — по железу: сильная машина → старшая модель, слабая → лёгкая;
//   • не хватает памяти прямо сейчас → Rust (plan_inference) сам снизит контекст
//     либо возьмёт модель полегче той же роли — уже на конкретном запросе.
// Ручной выбор в настройках («Модель для ответов») перекрывает ТЕКСТОВУЮ роль;
// ход с картинкой всё равно уйдёт на модель зрения — иначе на него нечем ответить.

// Лучшая УСТАНОВЛЕННАЯ модель роли под текущее железо. Сначала теги набора
// (порядок — по «светофору»: зелёный → старшая, иначе лёгкая), затем любая
// подходящая из установленных (пользователь мог поставить свою). "" — такой нет.
function bestOfRole(role: "text" | "vision"): string {
  const names = installed.map((m) => m.name);
  // Всё, что не зелёный уровень (жёлтый/красный/ещё не измерено), считаем слабым.
  const heavyFirst = autoTagsForRole(role);
  const order = state.hwTier === "green" ? heavyFirst : [...heavyFirst].reverse();
  for (const tag of order) if (names.includes(tag)) return tag;

  // Набор недоступен (движок не ответил на model_states) или из него ничего не
  // установлено: выбираем среди установленных по их же возможностям — эти признаки
  // приходят от движка, своего списка моделей тут по-прежнему нет.
  const isVision = (n: string) => visionByModel.get(n) ?? false;
  const isCode = (n: string) => /coder/i.test(n);
  const pool =
    role === "vision"
      ? names.filter(isVision)
      : names.filter((n) => !isVision(n) && !isCode(n));
  if (pool.length) {
    // При прочих равных предпочитаем семейство из набора — но только если набор
    // известен: захардкоженного имени семейства здесь быть не должно.
    const family = heavyFirst[0]?.split(":")[0];
    return (family && pool.find((n) => n.startsWith(family))) || pool[0];
  }
  // Роли нет вовсе. Текст: отвечаем хоть чем-то (модель зрения тоже умеет говорить).
  // Зрение: честное «нет» — attachments.ts предложит установить qwen3-vl.
  return role === "text" ? (names[0] ?? "") : "";
}

// Модель для хода с картинкой (chat.ts, attachments.ts). "" — модели зрения нет.
export function pickVisionModel(): string {
  return bestOfRole("vision");
}

// Применить выбор модели: ручной (если такая ещё установлена) либо авто-подбор.
// Идемпотентна и дёшева — зовут и loadModels (список изменился), и loadHardware
// (уточнён уровень железа), и настройки (пользователь сменил выбор).
export function applyModelChoice() {
  const names = installed.map((m) => m.name);
  // Выбранную вручную модель могли удалить — тогда честно возвращаемся к авто.
  if (state.modelChoice !== "auto" && !names.includes(state.modelChoice) && names.length) {
    state.modelChoice = "auto";
    invoke("set_setting", { key: "model_choice", value: "auto" }).catch(() => {});
  }
  const manual = state.modelChoice !== "auto" && names.includes(state.modelChoice);
  state.selectedModel = manual ? state.modelChoice : bestOfRole("text");

  // Во время генерации композером распоряжается setStreaming (поле ввода заблокировано);
  // не перебиваем его — иначе открытие настроек посреди ответа разблокирует ввод.
  if (!state.streaming) setComposerEnabled(Boolean(state.selectedModel));
  updateThinkAvailability();
  renderModelChoice(manual);
  renderActiveModel(manual);
}

// Дописать в подпись имя модели и следом — её тег: имя отвечает на вопрос «что это
// за модель», тег нужен поддержке. Тег мельче и тише (класс model-tag), а у моделей
// вне набора имя и есть тег — тогда не повторяем его дважды.
function appendModelName(box: HTMLElement, tag: string) {
  box.append(humanModel(tag));
  if (!titleByTag.has(tag)) return;
  const el = document.createElement("span");
  el.className = "model-tag";
  el.textContent = tag;
  box.append(" ", el);
}

// Поле «Модель для ответов» в настройках: «Автоматически» + установленные модели.
function renderModelChoice(manual: boolean) {
  modelChoiceEl.innerHTML = "";
  const auto = document.createElement("option");
  auto.value = "auto";
  auto.textContent = "Автоматически";
  modelChoiceEl.appendChild(auto);
  for (const m of installed) {
    const opt = document.createElement("option");
    opt.value = m.name;
    // В списке выбора имя и тег идут одной строкой (оформить часть строки внутри
    // <option> нельзя): выбирают по имени, тег остаётся для сверки.
    const title = titleByTag.get(m.name);
    opt.textContent = title ? `${title} · ${m.name}` : m.name;
    modelChoiceEl.appendChild(opt);
  }
  modelChoiceEl.value = manual ? state.modelChoice : "auto";
  modelChoiceEl.disabled = installed.length === 0;

  // Подпись переносится по словам (имя модели длиннее тега) — иначе хвост про
  // картинки обрезался бы многоточием.
  modelChoiceStateEl.classList.add("settings-field__value--wrap");
  if (!state.selectedModel) {
    modelChoiceStateEl.textContent = listFailed
      ? "движок не отвечает — список моделей недоступен"
      : "моделей нет — установите их ниже";
    return;
  }
  modelChoiceStateEl.textContent = "";
  modelChoiceStateEl.append(manual ? "вручную: " : "сейчас ");
  appendModelName(modelChoiceStateEl, state.selectedModel);
  const vis = pickVisionModel();
  if (vis && vis !== state.selectedModel) {
    modelChoiceStateEl.append(", с картинкой — ");
    appendModelName(modelChoiceStateEl, vis);
  }
}

// Тег живёт отдельной строкой под именем: карточка железа узкая, в одну строку имя
// и тег не помещаются. Колонку под них собираем один раз и дальше переиспользуем.
let hwModelTagEl: HTMLElement | null = null;
function hwModelTag(): HTMLElement {
  if (!hwModelTagEl) {
    const box = document.createElement("div");
    box.className = "hwchip__mbox";
    hwModelEl.insertBefore(box, hwModelNameEl);
    box.appendChild(hwModelNameEl); // имя переезжает в колонку, тег встаёт под ним
    hwModelTagEl = document.createElement("span");
    hwModelTagEl.className = "model-tag";
    box.appendChild(hwModelTagEl);
  }
  return hwModelTagEl;
}

// Строка активной модели в блоке железа (внизу слева) — единственное место, где
// модель видно на экране чата. Главное — человеческое имя («Базовая текстовая»),
// технический тег остаётся под ним тише и мельче: заказчику он не нужен, поддержке
// и диагностике — нужен.
function renderActiveModel(manual: boolean) {
  if (!state.selectedModel) {
    hwModelEl.hidden = true;
    return;
  }
  const human = humanModel(state.selectedModel);
  const tagEl = hwModelTag();
  hwModelNameEl.textContent = human;
  tagEl.textContent = state.selectedModel;
  tagEl.hidden = human === state.selectedModel; // модель вне набора: имя и есть тег
  const how = manual
    ? "Модель выбрана вручную в настройках"
    : "Модель подобрана автоматически — по задаче и по вашему железу";
  hwModelEl.title = `${human} · ${state.selectedModel}\n${how}`;
  hwModelEl.hidden = false;
}

// Тянет список установленных моделей из Ollama (Rust-команда list_models) и
// пересчитывает активную модель.
let warnedNoModels = false;
export async function loadModels() {
  let models: ModelInfo[];
  try {
    models = await invoke<ModelInfo[]>("list_models");
  } catch (err) {
    installed = [];
    listFailed = true;
    applyModelChoice(); // композер выключен, поле в настройках честно пустое
    addError(`Не удалось получить список моделей: ${humanError(err)}`);
    return;
  }

  installed = models;
  listFailed = false;
  thinkingByModel.clear();
  visionByModel.clear();
  toolsByModel.clear();
  for (const m of models) {
    thinkingByModel.set(m.name, m.thinking);
    visionByModel.set(m.name, m.vision);
    toolsByModel.set(m.name, m.tools);
  }
  await ensureModelTitles(); // имена набора — до первой отрисовки, без «скачка» подписи
  applyModelChoice();

  if (!state.selectedModel) {
    // Без команд терминала (принцип продукта): ведём в настройки или мастер.
    // Предупреждаем один раз, а не на каждый повтор проверки.
    if (!warnedNoModels) {
      warnedNoModels = true;
      addError(
        "Модели не установлены. Откройте Настройки → «Модели» и нажмите «Установить» " +
          "у нужной модели, либо запустите «Мастер установки…» — он всё сделает сам.",
      );
    }
    return;
  }
  warnedNoModels = false;
  inputEl.focus();
}

// Восстановить выбор модели из settings.json (по умолчанию — «Автоматически»).
// Прежний ключ selected_model (модель, выбранная в шапке) намеренно НЕ переносим:
// теперь модель подбирается сама, а старый выбор молча запирал бы пользователя
// на одной модели.
export async function initModelChoice() {
  try {
    const saved = await invoke<string | null>("get_setting", { key: "model_choice" });
    if (saved) state.modelChoice = saved;
  } catch {
    /* настройки недоступны — остаёмся на «Автоматически» */
  }
}

// Включает/выключает тумблер «Размышления» по возможностям активной модели.
export function updateThinkAvailability() {
  const supports = thinkingByModel.get(state.selectedModel) ?? false;
  thinkToggleEl.disabled = !supports;
  thinkToggleEl.title = supports
    ? "Режим рассуждений модели (медленнее, но точнее)"
    : "Эта модель не поддерживает режим рассуждений";
  setToggleState(thinkToggleEl, supports && state.thinkEnabled);
}

// ── Каталог набора моделей (страница настроек, M4) ───────────────────────────

// Итог операции в карточке «Модели» — единым компонентом статуса: успех гаснет
// сам, ошибка ждёт, пока её прочтут.
export function modelsStatus(text: string, kind: StatusKind = "info") {
  showStatus(modelsStatusEl, text, kind);
}

// Локальные состояния моделей набора (без сети) → отрисовка списка.
export async function loadModelStates() {
  try {
    const res = await invoke<{ models: ModelState[]; others: string[] }>("model_states");
    modelStates = res.models;
    // Имена набора приходят вместе с состояниями: если они уточнились, подписи с
    // именем модели (карточка железа, «Модель для ответов») перерисовываем.
    if (rememberTitles(res.models)) applyModelChoice();
  } catch (e) {
    modelStates = [];
    modelsStatus(`Не удалось получить список моделей: ${humanError(e)}`, "error");
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

// Вернуть кнопку к исходному виду. Подпись обязана совпадать с index.html: рядом
// в настройках есть такая же кнопка для обновлений САМОГО приложения, и без слова
// «моделей» после первой же проверки эти две кнопки станут неразличимы.
export function resetCheckButton() {
  if (checkBtnTimer) {
    clearTimeout(checkBtnTimer);
    checkBtnTimer = undefined;
  }
  checkUpdatesBtn.disabled = false;
  checkUpdatesBtn.classList.remove("check-ok", "check-update", "check-err", "checking");
  checkUpdatesBtn.innerHTML = `${ICON_REFRESH_CW}Проверить обновления моделей`;
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
      btn.className = "ep-btn ep-btn--icon ep-btn--primary"; // главное действие — акцентом
      btn.innerHTML = m.installed
        ? `${ICON_REFRESH_CW}Обновить`
        : `${ICON_DOWNLOAD}Установить`;
      btn.disabled = state.pullingTag !== null; // во время активной установки — заблокировано
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
    done: () => modelsStatus(`${tag}: ${isUpdate ? "обновлено" : "установлено"}.`, "success"),
    cancelled: () =>
      modelsStatus(
        `${tag}: ${isUpdate ? "обновление отменено" : "установка отменена"} — докачка продолжится при повторе.`,
        "info",
      ),
    error: (m) =>
      modelsStatus(`Не удалось ${isUpdate ? "обновить" : "установить"} ${tag}: ${m}`, "error"),
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

// ── «Светофор» железа ────────────────────────────────────────────────────────

// Словесная оценка + характеристики с подписями. Уровень и числа берём из
// detect_hardware (логику НЕ меняем). Уровень же задаёт вес авто-модели.
export async function loadHardware() {
  let hw: HardwareInfo;
  try {
    hw = await invoke<HardwareInfo>("detect_hardware");
  } catch {
    hwWordEl.textContent = "Конфигурация не определена";
    hwSpecsEl.textContent = "";
    hwBarEl.className = "hwchip hwchip--unknown";
    hwBarEl.hidden = false;
    // Железо неизвестно — берём осторожный (лёгкий) вариант модели, но выбор
    // всё равно применяем: иначе поле в настройках осталось бы неотрисованным.
    applyModelChoice();
    return;
  }

  // Цвет — у кружка (классы hwchip--*); статус — текстом в блоке.
  const word =
    hw.tier === "green" ? "Оптимально" : hw.tier === "yellow" ? "Достаточно" : "Ограничено";

  // Внутри характеристики — неразрывные пробелы (не рвётся); перенос только между ними.
  const nb = " ";
  const specs: string[] = [];
  // GPU — только при наличии выделенной видеопамяти (на unified/Apple Silicon vram_gb == null).
  // Рядом — честная свободная сейчас (если ОС её сообщает), а не только паспортный объём.
  if (hw.vram_gb != null) {
    const free = hw.vram_free_gb != null ? ` (свободно${nb}${hw.vram_free_gb.toFixed(1)})` : "";
    specs.push(`GPU${nb}${hw.vram_gb.toFixed(0)}${nb}ГБ${free}`);
  }
  specs.push(`RAM${nb}${hw.ram_gb.toFixed(0)}${nb}ГБ`);
  specs.push(`CPU${nb}${hw.cpu_cores}${nb}${plural(hw.cpu_cores, "ядро", "ядра", "ядер")}`);

  hwWordEl.textContent = word; // строка 1: статус
  hwSpecsEl.textContent = specs.join(" · "); // строка 2: характеристики (перенос по « · »)
  hwBarEl.className = `hwchip hwchip--${hw.tier}`;
  hwBarEl.hidden = false;

  // Уровень железа решает, какую модель берёт авто-выбор (старшую или лёгкую), —
  // пересчитываем активную модель (список моделей мог загрузиться раньше железа).
  state.hwTier = hw.tier;
  applyModelChoice();

  // Щадящий режим по умолчанию на слабом железе (yellow/red): бережём машину от
  // перегрева, пока пользователь сам не решил иначе (явный выбор в настройках
  // снимает авто-режим и дальше уважается).
  if (state.gentleAuto) {
    state.gentleMode = hw.tier !== "green";
    updateGentleUi();
  }
}

// ── Движок: обеспечение при старте и мягкая проверка ─────────────────────────

// Обеспечить движок при старте: приложение само переиспользует запущенную Ollama
// или поднимает свою (терминал пользователю не нужен). Возвращает, готов ли движок.
// Пока движок не подтвердил, что жив, подпись «на чём работает» не показываем:
// висящая версия при погасшем индикаторе выглядела бы как враньё.
function hideEngineMeta() {
  engineMetaEl.hidden = true;
  engineVersionEl.textContent = "";
  engineEl.removeAttribute("title");
}

export async function ensureEngine(): Promise<boolean> {
  statusEl.textContent = "Запуск движка…";
  hideEngineMeta();
  engineEl.classList.remove("engine--down");
  let res: { status: string; message: string };
  try {
    res = await invoke("ensure_engine");
  } catch (e) {
    statusEl.textContent = "Движок недоступен";
    engineEl.classList.add("engine--down");
    console.error("ensure_engine:", e);
    return false;
  }
  if (res.status === "ready") return true; // checkOllama ниже покажет версию
  // not_installed / error — показываем понятный статус, дальнейшие шаги пропустим.
  //
  // res.message выбрасывался: в шапке оставались два слова, а единственное
  // объяснение, что именно не так и куда идти, приходило из Rust и терялось.
  // Пользователь видел «Движок не установлен» и не знал, что делать дальше.
  statusEl.textContent =
    res.status === "not_installed" ? "Движок не установлен" : "Движок не запущен";
  if (res.message) {
    statusEl.title = res.message;
    addError(res.message);
  }
  engineEl.classList.add("engine--down");
  return false;
}

// Мягкая проверка движка. В шапке — сначала человеческое состояние («Готов к работе»),
// и тихой серой подписью рядом — на чём именно оно работает: «Ollama 0.30.8». Заказчик
// видит и что всё в порядке, и что под капотом (это часть доверия: движок локальный).
// Неблокирующая — при недоступности просто показываем статус, приложение работает.
export async function checkOllama() {
  try {
    const version = await invoke<string>("ollama_version");
    statusEl.textContent = "Готов к работе";
    engineVersionEl.textContent = version;
    engineMetaEl.hidden = false;
    engineEl.title =
      `Нейросеть работает прямо на этом компьютере — через локальный движок Ollama ${version}. ` +
      "Ничего не уходит в интернет.";
    engineEl.classList.remove("engine--down");
  } catch {
    statusEl.textContent = "Движок не отвечает";
    hideEngineMeta();
    engineEl.classList.add("engine--down");
  }
}

// Кнопка «обновить» в шапке: полная перепроверка — движок, железо и список моделей
// (подхватывает только что скачанные модели и смену железа без перезапуска).
async function refreshAll() {
  refreshBtn.disabled = true;
  try {
    await checkOllama();
    await Promise.all([loadHardware(), loadModels()]);
  } finally {
    refreshBtn.disabled = false;
  }
}

// Обработчики: ручной выбор модели (настройки), «обновить», проверка обновлений,
// отмена установки.
export function wireModels() {
  modelChoiceEl.addEventListener("change", () => {
    state.modelChoice = modelChoiceEl.value || "auto";
    applyModelChoice(); // у новой модели другие возможности (размышления, зрение)
    // запоминаем выбор между запусками (settings.json — единый источник истины)
    invoke("set_setting", { key: "model_choice", value: state.modelChoice }).catch((e) =>
      console.error("set_setting model_choice:", e),
    );
  });
  refreshBtn.addEventListener("click", refreshAll);
  checkUpdatesBtn.addEventListener("click", checkModelUpdates);
  modelPullCancelBtn.addEventListener("click", cancelActivePull);
}
