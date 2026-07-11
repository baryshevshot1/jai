// Страница настроек и её разделы: открытие/закрытие, офлайн-поставка (пути движка
// и каталога моделей), диагностика «Проверка системы», обновления приложения,
// а также пользовательские предпочтения — тема, «Размышления», боковая панель.

import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { check, type Update } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";
import { getVersion } from "@tauri-apps/api/app";
import type { DiagCheck } from "./types";
import { state } from "./state";
import {
  appEl,
  appUpdateCheckBtn,
  appUpdateDiskBtn,
  appUpdateInfoEl,
  appUpdateProgressEl,
  appUpdateProgressFill,
  appUpdateProgressLabel,
  appUpdateStatusEl,
  appVersionEl,
  composerWrapEl,
  diagListEl,
  diagRunBtn,
  epEngineEl,
  epModelsEl,
  epResetBtn,
  epSetEngineBtn,
  epSetModelsBtn,
  feedEl,
  inputEl,
  installEmbedBtn,
  installFromDiskBtn,
  installLocalBtn,
  projectView,
  settingsBackBtn,
  settingsBtn,
  settingsStatusEl,
  settingsView,
  sidebarResizer,
  sidebarToggleBtn,
  themeBtn,
  themeIconEl,
  thinkToggleEl,
} from "./dom";
import { humanError, ICON_REFRESH_CW } from "./util";
import { confirmModal, showChatView } from "./ui";
import {
  flashIndexLabel,
  refreshDocuments,
  showIndexProgress,
  sidebarDocCtx,
} from "./documents";
import { loadModelStates, loadModels, modelsStatus, resetCheckButton } from "./models";
import { closeProjectView } from "./projects";
import { refreshOutboundLog } from "./online";

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
  showChatView();
  resetCheckButton(); // уход со страницы — кнопка к исходному (итог в сессии сохранён)
  if (!state.streaming && state.selectedModel) inputEl.focus();
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
// Обновление зависимых экранов (список документов, модели) — на вызывающем; статус
// bge-m3 для честного сообщения перечитываем сами.
async function applyModelsDir(dir: string, report: (t: string, err: boolean) => void) {
  try {
    await invoke("set_models_dir", { path: dir }); // валидация manifests/blobs + запись
    const res = await invoke<{ status: string; message: string }>("reload_engine");
    let ready = false;
    try {
      ready = await invoke<boolean>("embedding_status");
    } catch {
      ready = false;
    }
    state.embeddingReady = ready;
    if (res.status === "external") report(res.message, false);
    else if (ready) report("Локальный каталог моделей применён", false);
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
  await refreshDocuments(sidebarDocCtx);
  await loadModels();
  installEmbedBtn.disabled = false;
  installLocalBtn.disabled = false;
}

// «Указать…» каталог моделей со страницы настроек — обратная связь там же.
async function settingsPickModels() {
  const dir = await pickModelsDir();
  if (!dir) return;
  settingsStatus("Применение локального каталога…", false);
  await applyModelsDir(dir, settingsStatus);
  await refreshDocuments(sidebarDocCtx);
  await loadModels();
}

// Источник «с диска» из раздела «Модели»: указать локальный каталог моделей.
async function installFromDiskForModels() {
  const dir = await pickModelsDir();
  if (!dir) return;
  modelsStatus("Применение локального каталога…", false);
  await applyModelsDir(dir, modelsStatus);
  await refreshDocuments(sidebarDocCtx);
  await loadModels();
  await loadModelStates();
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

// ── Проверка системы (диагностика): раздел настроек ──────────────────────────

// Иконки проверок — в стиле иконок ролей моделей (контурные, 24×24).
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

// ── Тема (светлая/тёмная). Выбор хранится через Tauri (settings.json) ────────

// Иконки: показываем действие-противоположность (в тёмной — солнце, в светлой — луна).
const ICON_SUN =
  '<circle cx="12" cy="12" r="4"/><path d="M12 2v3M12 19v3M2 12h3M19 12h3M5 5l2 2M17 17l2 2M19 5l-2 2M7 17l-2 2"/>';
const ICON_MOON = '<path d="M21 12.8A9 9 0 1 1 11.2 3a7 7 0 0 0 9.8 9.8z"/>';

function applyTheme(theme: string) {
  document.documentElement.setAttribute("data-theme", theme);
  themeIconEl.innerHTML = theme === "dark" ? ICON_SUN : ICON_MOON;
}

// При старте: тема из настроек Tauri; если не сохранена — по системной.
export async function initTheme() {
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

// ── Тумблер «Размышления» ────────────────────────────────────────────────────

// Восстановление тумблера «Размышления» из settings.json (единый источник истины).
// По умолчанию ВЫКЛ: с reasoning-моделью даже простые вопросы думаются по ~20 секунд.
// Одноразовая миграция из прежнего хранилища localStorage("jai.think"), чтобы выбор
// пользователя не потерялся; затем localStorage для этой настройки не используется.
export async function initThinking() {
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
  state.thinkEnabled = saved === "true"; // null/"false" → ВЫКЛ
  thinkToggleEl.classList.toggle("on", state.thinkEnabled);
  thinkToggleEl.addEventListener("click", () => {
    state.thinkEnabled = !state.thinkEnabled;
    thinkToggleEl.classList.toggle("on", state.thinkEnabled);
    invoke("set_setting", { key: "thinking_enabled", value: String(state.thinkEnabled) }).catch((e) =>
      console.error("set_setting thinking_enabled:", e),
    );
  });
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
export async function initSidebar() {
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

// Обработчики страницы настроек, темы, панели, диагностики и обновлений.
export function wireSettings() {
  settingsBtn.addEventListener("click", openSettings);
  settingsBackBtn.addEventListener("click", closeSettings);
  epSetModelsBtn.addEventListener("click", settingsPickModels);
  epSetEngineBtn.addEventListener("click", setEnginePathDialog);
  epResetBtn.addEventListener("click", resetEnginePaths);
  installLocalBtn.addEventListener("click", installFromLocalDir);
  installFromDiskBtn.addEventListener("click", installFromDiskForModels);
  themeBtn.addEventListener("click", toggleTheme);
  sidebarResizer.addEventListener("pointerdown", startSidebarResize);
  sidebarToggleBtn.addEventListener("click", toggleSidebar);
  diagRunBtn.addEventListener("click", runDiagnostics);
  appUpdateCheckBtn.addEventListener("click", checkAppUpdate);
  appUpdateDiskBtn.addEventListener("click", installAppUpdateFromDisk);
  getVersion()
    .then((v) => (appVersionEl.textContent = v))
    .catch(() => (appVersionEl.textContent = "—"));
}
