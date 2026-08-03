// База документов: список/добавление/удаление документов, индикатор прогресса
// индексации и установка модели эмбеддингов (bge-m3). Одна логика обслуживает
// и карточку «Документы» в настройках (общая база быстрых чатов, projectId=null),
// и раздел «Знания проекта» — через объект DocCtx.

import { Channel, invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import type { DocCtx, DocumentMeta, IndexProgress, PullOutcome } from "./types";
import { state, tagForRole } from "./state";
import {
  addDocBtn,
  docListEl,
  docStatusEl,
  docStatusTextEl,
  indexProgressEl,
  indexProgressFill,
  indexProgressLabel,
  installEmbedBtn,
  installLocalBtn,
  projectAddDocBtn,
  projectDocListEl,
  projectIndexProgressEl,
  projectIndexProgressFill,
  projectIndexProgressLabel,
  pullCancelBtn,
} from "./dom";
import { fileFormat, humanError, plural } from "./util";
import { confirmModal } from "./ui";
import { cancelActivePull, runPull } from "./pull";

export let sidebarDocCtx: DocCtx; // общие документы (сайдбар), projectId всегда null
export let projectDocCtx: DocCtx; // знания проекта (экран проекта), projectId — текущий проект

// Контексты документов: сайдбар (общие, вне проектов) и экран проекта.
// Вызывается из main.ts сразу после initDom().
export function initDocContexts() {
  sidebarDocCtx = {
    projectId: null,
    listEl: docListEl,
    progressEl: indexProgressEl,
    fillEl: indexProgressFill,
    labelEl: indexProgressLabel,
    addBtn: addDocBtn,
    flashTimer: null,
    opGen: 0,
  };
  projectDocCtx = {
    projectId: null,
    listEl: projectDocListEl,
    progressEl: projectIndexProgressEl,
    fillEl: projectIndexProgressFill,
    labelEl: projectIndexProgressLabel,
    addBtn: projectAddDocBtn,
    flashTimer: null,
    opGen: 0,
  };
}

// Тянет список документов контекста и (для общей базы) статус модели эмбеддингов.
export async function refreshDocuments(ctx: DocCtx) {
  // «Не удалось спросить» и «модели нет» — разные вещи: пустая база не должна
  // советовать установить bge-m3, если на самом деле молчит движок.
  let statusUnknown = false;
  try {
    state.embeddingReady = await invoke<boolean>("embedding_status");
  } catch {
    state.embeddingReady = false;
    statusUnknown = true;
  }
  // Карточка установки bge-m3 — только у общей базы (общий статус модели поиска).
  if (ctx === sidebarDocCtx) {
    if (state.embeddingReady) {
      docStatusEl.hidden = true;
    } else if (state.pullingTag === null) {
      docStatusEl.hidden = false;
      docStatusTextEl.textContent = statusUnknown
        ? "Проверить модель поиска bge-m3 не удалось: движок не отвечает. Запустите его в настройках — или, если модели ещё нет, скачайте из интернета либо импортируйте с флешки/диска."
        : "Для поиска по документам нужна модель bge-m3. Скачайте из интернета или импортируйте с флешки/диска (каталог моделей Ollama) — без терминала.";
      installEmbedBtn.hidden = false;
      installEmbedBtn.disabled = false;
      installEmbedBtn.textContent = "Установить (~1.2 ГБ)"; // единый глагол установки
      installLocalBtn.hidden = false;
      installLocalBtn.disabled = false;
    }
  }

  let docs: DocumentMeta[] = [];
  let listFailed = false;
  try {
    docs = await invoke<DocumentMeta[]>("list_documents", { projectId: ctx.projectId });
  } catch (e) {
    // Молча показать пустой список нельзя: «База пуста» вместо непрочитанного
    // списка читается как «мои документы пропали».
    listFailed = true;
    hideIndexProgress(ctx); // чтение списка идёт без прогресса — полоса ни при чём
    flashIndexLabel(`Не удалось прочитать список документов: ${humanError(e)}`, true, ctx);
  }
  renderDocList(docs, ctx, { listFailed, statusUnknown });
}

function renderDocList(
  docs: DocumentMeta[],
  ctx: DocCtx,
  trouble: { listFailed: boolean; statusUnknown: boolean },
) {
  ctx.listEl.innerHTML = "";
  if (docs.length === 0) {
    const empty = document.createElement("div");
    empty.className = "doc-empty";
    empty.textContent = trouble.listFailed
      ? "Список документов прочитать не удалось. Это не значит, что документы удалены: откройте раздел заново или перезапустите программу."
      : trouble.statusUnknown
        ? "Движок не отвечает — проверить модель поиска не удалось. Запустите движок в настройках и откройте раздел заново."
        : !state.embeddingReady
          ? "Для документов нужна модель поиска bge-m3 — установите её в настройках (раздел «Документы») или через мастер установки."
          : ctx.projectId
            ? "В проекте пока нет документов. Добавьте — и чаты проекта будут искать по ним."
            : "База пуста. Добавьте документы — и спрашивайте по ним.";
    ctx.listEl.appendChild(empty);
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
    del.addEventListener("click", () => deleteDocument(d, ctx));
    item.appendChild(del);

    ctx.listEl.appendChild(item);
  }
}

export function showIndexProgress(label: string, frac: number, ctx: DocCtx) {
  if (ctx.flashTimer !== null) {
    clearTimeout(ctx.flashTimer);
    ctx.flashTimer = null;
  }
  ctx.progressEl.hidden = false;
  ctx.fillEl.style.width = `${Math.round(frac * 100)}%`;
  ctx.fillEl.classList.remove("error"); // новая операция начинается с чистой полосы
  ctx.labelEl.textContent = label;
  ctx.labelEl.classList.remove("danger");
}

export function hideIndexProgress(ctx: DocCtx) {
  ctx.progressEl.hidden = true;
  ctx.fillEl.style.width = "0";
  ctx.fillEl.classList.remove("error");
}

// Итог операции в области прогресса. Успех сам гаснет через несколько секунд —
// его прочитали, и он больше не нужен. Сбой остаётся на месте, пока его не
// сменит следующая операция (showIndexProgress чистит и надпись, и полосу):
// исчезнувшая сама собой надпись об ошибке читается как «всё получилось», а в
// журнал за подробностями пользователь не пойдёт. Полоса при сбое замирает там,
// где остановилась, и краснеет — залитая до конца выглядела бы как успех.
export function flashIndexLabel(text: string, isError: boolean, ctx: DocCtx) {
  if (ctx.flashTimer !== null) {
    clearTimeout(ctx.flashTimer);
    ctx.flashTimer = null;
  }
  ctx.progressEl.hidden = false;
  ctx.labelEl.textContent = text;
  ctx.labelEl.classList.toggle("danger", isError);
  ctx.fillEl.classList.toggle("error", isError);
  if (isError) return;
  ctx.fillEl.style.width = "0";
  ctx.flashTimer = window.setTimeout(() => {
    ctx.progressEl.hidden = true;
    ctx.flashTimer = null;
  }, 3500);
}

// «Добавить документ» в контекст (сайдбар или проект): выбор файла → индексация.
export async function addDocument(ctx: DocCtx) {
  let path: string | null;
  try {
    const sel = await open({
      multiple: false,
      filters: [{ name: "Документы", extensions: ["pdf", "docx", "txt", "md"] }],
    });
    path = typeof sel === "string" ? sel : null;
  } catch (e) {
    hideIndexProgress(ctx); // выбор файла не начинал индексацию — полоса пустая
    flashIndexLabel(`Не удалось открыть окно выбора файла: ${humanError(e)}`, true, ctx);
    return;
  }
  if (!path) return;

  ctx.addBtn.disabled = true;
  const myOp = ++ctx.opGen; // виджет прогресса принадлежит этой операции (как myGen в send)
  showIndexProgress("Чтение документа…", 0.04, ctx);

  const onProgress = new Channel<IndexProgress>();
  onProgress.onmessage = (p) => {
    if (myOp !== ctx.opGen) return; // операция уже завершена/сменилась — хвост не рисуем
    const frac = p.total ? p.current / p.total : 0;
    if (p.phase === "chunk") showIndexProgress(`Подготовка фрагментов: ${p.total}`, 0.08, ctx);
    else if (p.phase === "embed") showIndexProgress(`Индексация: ${p.current} из ${p.total}`, frac, ctx);
    else if (p.phase === "done") showIndexProgress("Сохранение…", 1, ctx);
  };

  try {
    const res = await invoke<{ status: string; document: DocumentMeta; rebuilt: boolean }>(
      "index_document",
      { path, projectId: ctx.projectId, onProgress },
    );
    ctx.opGen++; // операция завершена: запоздавший progress ЕЁ ЖЕ канала не затрёт итог
    await refreshDocuments(ctx);
    // ВСЕГДА, а не только для проекта. Условие `if (ctx.projectId)` тихо ломало
    // главную функцию продукта: документ, добавленный в ОБЩУЮ базу (её projectId
    // равен null), не поднимал флаг у открытого чата, а чат ищет по базе только
    // при docsCount > 0. Человек добавлял договор, видел «Добавлен: договор.docx»,
    // тут же спрашивал по нему — и получал ответ без документа, без источников и
    // без единого сообщения о том, что поиск не выполнялся.
    //
    // Один лишний вызов дешевле любого условия: удаление ниже так и делает.
    await refreshCurrentDocsCount();
    if (res.rebuilt) {
      flashIndexLabel("База пересоздана под новую модель поиска — прежние документы добавьте заново", true, ctx);
    } else if (res.status === "exists") {
      flashIndexLabel(`«${res.document.filename}» уже в базе`, false, ctx);
    } else {
      flashIndexLabel(`Добавлен: ${res.document.filename}`, false, ctx);
    }
  } catch (e) {
    ctx.opGen++;
    // Полосу не гасим: она замирает там, где индексация оборвалась, и краснеет —
    // видно, что документ НЕ добавлен, а не «мигнуло и пропало».
    flashIndexLabel(humanError(e), true, ctx);
  } finally {
    ctx.addBtn.disabled = false;
  }
}

async function deleteDocument(d: DocumentMeta, ctx: DocCtx) {
  if (!(await confirmModal(`Удалить «${d.filename}» из базы документов?`))) return;
  try {
    await invoke("delete_document", { id: d.id });
  } catch (e) {
    hideIndexProgress(ctx); // удаление идёт без прогресса — полоса ни при чём
    flashIndexLabel(`Не удалось удалить «${d.filename}»: ${humanError(e)}`, true, ctx);
    return;
  }
  await refreshDocuments(ctx);
  await refreshCurrentDocsCount();
}

// Есть ли документы в базе ОТКРЫТОГО чата (его проекта или общей — вне проектов).
// Определяет, запускать ли RAG-поиск перед ответом (docsCount как флаг 0/1).
export async function refreshCurrentDocsCount() {
  try {
    const empty = await invoke<boolean>("documents_empty", { projectId: state.currentProjectId });
    state.docsCount = empty ? 0 : 1;
  } catch (e) {
    // Не смогли спросить — считаем, что искать нечего (поиск не сорвёт ответ).
    // Своей строки состояния у этой проверки нет: если документы действительно
    // есть, о сбое честно скажет список документов и сам ответ без источников.
    console.error("documents_empty:", e);
    state.docsCount = 0;
  }
}

// «Установить bge-m3»: единый runPull с прогрессом в карточке «Документы» настроек.
// Возвращает итог: обновление списка МОДЕЛЕЙ после успеха делает точка сборки
// (main.ts) — documents не импортирует models (слои без циклов).
export async function installEmbeddingModel(): Promise<PullOutcome | "error"> {
  const ctx = sidebarDocCtx;
  const myOp = ++ctx.opGen; // виджет прогресса теперь принадлежит этой операции
  installEmbedBtn.hidden = true;
  docStatusEl.hidden = true; // на месте карточки — прогресс
  pullCancelBtn.hidden = false;
  pullCancelBtn.disabled = false;
  showIndexProgress("Подготовка установки…", 0.02, ctx);

  // Тег модели поиска берём из набора в Rust; литерал — только на случай, когда
  // набор ещё не получен (движок молчал), чтобы кнопка не оказалась мёртвой.
  const embedTag = tagForRole("embed") || "bge-m3";
  const outcome = await runPull(embedTag, {
    progress: (t, f) => {
      if (myOp === ctx.opGen) showIndexProgress(t, f, ctx);
    },
    done: () => {
      if (myOp === ctx.opGen) flashIndexLabel("Модель bge-m3 установлена", false, ctx);
    },
    cancelled: () => {
      if (myOp === ctx.opGen)
        flashIndexLabel(
          "Установка отменена — можно докачать позже (Ollama продолжит с места)",
          false,
          ctx,
        );
    },
    error: (m) => {
      if (myOp === ctx.opGen) flashIndexLabel(m, true, ctx);
    },
  });

  pullCancelBtn.hidden = true;
  if (outcome === "done") {
    // модель появилась — обновляем статус базы без перезапуска
    await refreshDocuments(ctx);
  } else if (!state.embeddingReady) {
    // не установилась — вернуть карточку с кнопкой
    docStatusEl.hidden = false;
    installEmbedBtn.hidden = false;
    installEmbedBtn.disabled = false;
  }
  return outcome;
}

// Обработчики карточки «Документы» (кнопка установки bge-m3 подключается в main.ts —
// ей после успеха нужен loadModels из models.ts, а documents его не импортирует).
export function wireDocuments() {
  addDocBtn.addEventListener("click", () => addDocument(sidebarDocCtx));
  pullCancelBtn.addEventListener("click", cancelActivePull);
}
