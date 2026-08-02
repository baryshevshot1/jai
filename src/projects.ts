// Проекты: список в сайдбаре, экран проекта (знания + чаты + инструкции).
// Списки чатов проекта и чип в шапке рисует conversations.ts (см. слои).

import { invoke } from "@tauri-apps/api/core";
import type { Project } from "./types";
import { state } from "./state";
import {
  chatProjectChip,
  projectAddDocBtn,
  projectBackBtn,
  projectDeleteBtn,
  projectInstructionsEl,
  projectListEl,
  projectNameInput,
  projectNewChatBtn,
  projectStatusEl,
  newProjectBtn,
} from "./dom";
import { humanError, makePressable } from "./util";
import { addError, confirmModal, promptModal, stop, showScreen} from "./ui";
import { addDocument, projectDocCtx, refreshDocuments } from "./documents";
import {
  newDialog,
  refreshConversationList,
  renderProjectChats,
} from "./conversations";

const ICON_PROJECT =
  '<path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/>';

// Тянет список проектов из бэкенда и рисует в боковой панели.
export async function refreshProjects() {
  try {
    state.projects = await invoke<Project[]>("list_projects");
  } catch (e) {
    console.error("list_projects:", e);
    state.projects = [];
  }
  renderProjectList();
}

function renderProjectList() {
  projectListEl.innerHTML = "";
  if (state.projects.length === 0) {
    const empty = document.createElement("div");
    empty.className = "project-empty";
    empty.textContent = "Проектов пока нет";
    projectListEl.appendChild(empty);
    return;
  }
  for (const p of state.projects) {
    const item = document.createElement("div");
    item.className = "project" + (p.id === state.viewingProjectId ? " active" : "");
    item.innerHTML = `<svg viewBox="0 0 24 24">${ICON_PROJECT}</svg>`;

    const name = document.createElement("span");
    name.className = "name";
    name.textContent = p.name || "Без названия";
    item.appendChild(name);

    item.addEventListener("click", () => openProjectView(p.id));
    makePressable(item); // открывается и с клавиатуры (Tab + Enter)
    projectListEl.appendChild(item);
  }
}

// Создать проект: спрашиваем имя, сохраняем, открываем его экран.
async function createProject() {
  const name = await promptModal("Название проекта", "Например: Договоры с поставщиками");
  if (name === null) return;
  const trimmed = name.trim() || "Новый проект";
  const now = Date.now();
  const proj: Project = { id: crypto.randomUUID(), name: trimmed, instructions: "", created_at: now, updated_at: now };
  try {
    await invoke("save_project", { project: proj });
  } catch (e) {
    addError(`Не удалось создать проект: ${humanError(e)}`);
    return;
  }
  await refreshProjects();
  openProjectView(proj.id);
}

// Сохранить имя/инструкции проекта (upsert). Тихо — это фоновое сохранение.
async function saveProjectMeta(proj: Project) {
  proj.updated_at = Date.now();
  try {
    await invoke("save_project", { project: proj });
    await refreshProjects();
  } catch (e) {
    projectStatus(`Не удалось сохранить: ${humanError(e)}`, true);
  }
}

function projectStatus(text: string, isError: boolean) {
  projectStatusEl.hidden = false;
  projectStatusEl.textContent = text;
  projectStatusEl.classList.toggle("settings-status--error", isError);
  window.setTimeout(() => (projectStatusEl.hidden = true), 3500);
}

// Открыть полноэкранный экран проекта: инструкции, знания (документы), чаты проекта.
export function openProjectView(id: string) {
  const proj = state.projects.find((p) => p.id === id);
  if (!proj) return;
  if (state.streaming) stop();
  state.viewingProjectId = id;
  showScreen("project"); // лента, настройки и мастер гаснут сами

  projectNameInput.value = proj.name;
  projectInstructionsEl.value = proj.instructions;
  projectStatusEl.hidden = true;

  projectDocCtx.projectId = id;
  refreshDocuments(projectDocCtx);
  renderProjectChats(id);
  renderProjectList(); // подсветить активный
}

export function closeProjectView() {
  showScreen("chat"); // снимет и выбор проекта, и подсветку строки в панели
  renderProjectList();
}

// Удалить проект вместе с чатами и документами (каскад на бэкенде).
async function deleteProjectFlow() {
  if (!state.viewingProjectId) return;
  const proj = state.projects.find((p) => p.id === state.viewingProjectId);
  const ok = await confirmModal(
    `Удалить проект «${proj?.name ?? ""}» со всеми его чатами и документами? Это необратимо.`,
  );
  if (!ok) return;
  const id = state.viewingProjectId;
  try {
    await invoke("delete_project", { id });
  } catch (e) {
    addError(`Не удалось удалить проект: ${humanError(e)}`);
    return;
  }
  // Если открытый чат принадлежал этому проекту — уводим в новый быстрый чат.
  if (state.currentProjectId === id) newDialog(null);
  closeProjectView();
  await refreshProjects();
  await refreshConversationList();
}

// Обработчики: создание/открытие/удаление проекта, имя и инструкции, чип в шапке.
export function wireProjects() {
  newProjectBtn.addEventListener("click", createProject);
  projectBackBtn.addEventListener("click", closeProjectView);
  projectDeleteBtn.addEventListener("click", deleteProjectFlow);
  projectAddDocBtn.addEventListener("click", () => addDocument(projectDocCtx));
  projectNewChatBtn.addEventListener("click", () => {
    if (state.viewingProjectId) newDialog(state.viewingProjectId);
  });
  chatProjectChip.addEventListener("click", () => {
    if (state.currentProjectId) openProjectView(state.currentProjectId);
  });
  // Имя проекта — сохраняем при потере фокуса/Enter.
  const commitProjectName = () => {
    if (!state.viewingProjectId) return;
    const proj = state.projects.find((p) => p.id === state.viewingProjectId);
    if (!proj) return;
    const name = projectNameInput.value.trim() || "Без названия";
    if (name !== proj.name) {
      proj.name = name;
      saveProjectMeta(proj);
    }
  };
  projectNameInput.addEventListener("blur", commitProjectName);
  projectNameInput.addEventListener("keydown", (e) => {
    if (e.key === "Enter") projectNameInput.blur();
  });
  // Инструкции проекта — сохраняем при потере фокуса.
  projectInstructionsEl.addEventListener("blur", () => {
    if (!state.viewingProjectId) return;
    const proj = state.projects.find((p) => p.id === state.viewingProjectId);
    if (!proj) return;
    if (projectInstructionsEl.value !== proj.instructions) {
      proj.instructions = projectInstructionsEl.value;
      saveProjectMeta(proj);
      projectStatus("Инструкции сохранены", false);
    }
  });
}
