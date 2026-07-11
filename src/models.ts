// Модели и движок: выпадающий список в шапке, каталог набора в настройках
// (состояния, установка/обновление, проверка обновлений), «светофор» железа,
// обеспечение/проверка движка Ollama.

import { invoke } from "@tauri-apps/api/core";
import type { HardwareInfo, ModelInfo, ModelState } from "./types";
import { state, thinkingByModel, toolsByModel, updateByTag, visionByModel } from "./state";
import {
  checkUpdatesBtn,
  engineEl,
  hwBarEl,
  hwModelEl,
  hwModelNameEl,
  hwSpecsEl,
  hwWordEl,
  inputEl,
  modelListEl,
  modelProgressEl,
  modelProgressFill,
  modelProgressLabel,
  modelPullCancelBtn,
  modelSelectEl,
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
import { addError, setComposerEnabled } from "./ui";
import { cancelActivePull, runPull } from "./pull";
import { refreshDocuments, sidebarDocCtx } from "./documents";

let modelStates: ModelState[] = [];

// ── Список моделей в шапке ───────────────────────────────────────────────────

// Тянет список установленных моделей из Ollama (через Rust-команду list_models)
// и заполняет выпадающий список в шапке.
export async function loadModels() {
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
  if (!state.selectedModel || !names.includes(state.selectedModel)) {
    let saved: string | null = null;
    if (!state.selectedModel) {
      try {
        saved = await invoke<string | null>("get_setting", { key: "selected_model" });
      } catch {
        saved = null;
      }
    }
    if (saved && names.includes(saved)) {
      state.selectedModel = saved; // сохранённая модель ещё установлена
    } else {
      const preferred = "qwen3.5:9b"; // откат: целевая, иначе первая в списке
      state.selectedModel = names.includes(preferred) ? preferred : names[0];
    }
  }
  modelSelectEl.value = state.selectedModel;
  modelSelectEl.disabled = false;
  setComposerEnabled(true);
  updateThinkAvailability();
  inputEl.focus();
}

// Включает/выключает тумблер «Размышления» по возможностям выбранной модели.
export function updateThinkAvailability() {
  const supports = thinkingByModel.get(state.selectedModel) ?? false;
  thinkToggleEl.disabled = !supports;
  thinkToggleEl.title = supports
    ? "Режим рассуждений модели (медленнее, но точнее)"
    : "Эта модель не поддерживает режим рассуждений";
  thinkToggleEl.classList.toggle("on", supports && state.thinkEnabled);
}

// Показывает в списке одиночную подсказку и блокирует ввод (нет моделей / нет Ollama).
function showModelHint(text: string) {
  modelSelectEl.innerHTML = "";
  const opt = document.createElement("option");
  opt.textContent = text;
  modelSelectEl.appendChild(opt);
  modelSelectEl.disabled = true;
  state.selectedModel = "";
  setComposerEnabled(false);
}

// ── Каталог набора моделей (страница настроек, M4) ───────────────────────────

let modelsStatusTimer: number | undefined;
export function modelsStatus(text: string, isError: boolean) {
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
export async function loadModelStates() {
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
export function resetCheckButton() {
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

// ── «Светофор» железа ────────────────────────────────────────────────────────

// Словесная оценка + характеристики с подписями. Уровень, числа и рекомендуемую
// модель берём из detect_hardware (логику НЕ меняем).
export async function loadHardware() {
  let hw: HardwareInfo;
  try {
    hw = await invoke<HardwareInfo>("detect_hardware");
  } catch {
    hwWordEl.textContent = "Конфигурация не определена";
    hwSpecsEl.textContent = "";
    hwModelEl.hidden = true;
    hwBarEl.className = "hwchip hwchip--unknown";
    hwBarEl.hidden = false;
    return;
  }

  // Цвет — у кружка (классы hwchip--*); статус и рекомендованная модель — текстом в блоке.
  const word =
    hw.tier === "green" ? "Оптимально" : hw.tier === "yellow" ? "Достаточно" : "Ограничено";
  const model = hw.tier === "green" ? "qwen3.5:9b" : "qwen3.5:4b";

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
  hwModelNameEl.textContent = `Рекомендуется ${model}`; // строка 3: модель — прямо в блоке
  hwModelEl.hidden = false;
  hwBarEl.className = `hwchip hwchip--${hw.tier}`;
  hwBarEl.hidden = false;
}

// ── Движок: обеспечение при старте и мягкая проверка ─────────────────────────

// Обеспечить движок при старте: приложение само переиспользует запущенную Ollama
// или поднимает свою (терминал пользователю не нужен). Возвращает, готов ли движок.
export async function ensureEngine(): Promise<boolean> {
  statusEl.textContent = "Запуск движка…";
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
  // not_installed / error — показываем понятный статус, дальнейшие шаги пропустим
  statusEl.textContent =
    res.status === "not_installed" ? "Движок не установлен" : "Движок не запущен";
  engineEl.classList.add("engine--down");
  return false;
}

// Мягкая проверка движка: спрашиваем версию Ollama и показываем её в шапке.
// Неблокирующая — при недоступности просто показываем статус, приложение работает.
export async function checkOllama() {
  try {
    const version = await invoke<string>("ollama_version");
    statusEl.textContent = `Ollama ${version}`;
    engineEl.classList.remove("engine--down");
  } catch {
    statusEl.textContent = "Ollama недоступна";
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

// Обработчики: выбор модели, «обновить», проверка обновлений, отмена pull.
export function wireModels() {
  modelSelectEl.addEventListener("change", () => {
    state.selectedModel = modelSelectEl.value;
    updateThinkAvailability(); // у новой модели могут быть другие возможности
    // запоминаем выбор между запусками (settings.json — единый источник истины)
    invoke("set_setting", { key: "selected_model", value: state.selectedModel }).catch((e) =>
      console.error("set_setting selected_model:", e),
    );
  });
  refreshBtn.addEventListener("click", refreshAll);
  checkUpdatesBtn.addEventListener("click", checkModelUpdates);
  modelPullCancelBtn.addEventListener("click", cancelActivePull);
}
