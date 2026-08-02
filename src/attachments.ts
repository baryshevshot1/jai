// Прикрепления в композере: документ (Фаза A) и изображение (зрение, qwen3-vl),
// подбор/установка vision-модели.

import { invoke } from "@tauri-apps/api/core";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { open } from "@tauri-apps/plugin-dialog";
import type { DocAttachment } from "./types";
import { autoTagsForRole, state, visionByModel } from "./state";
import {
  attachBtn,
  composerWrapEl,
  docChipBadgeEl,
  docChipEl,
  docChipNameEl,
  docRemoveBtn,
  imageBtn,
  imgChipEl,
  imgChipThumb,
  imgRemoveBtn,
  inputEl,
  messagesEl,
} from "./dom";
import { docSubline, fileFormat, imageDataUrl, plural } from "./util";
import {
  activeScreen,
  addError,
  addNotice,
  confirmModal,
  refreshEmptyState,
  scrollToBottom,
} from "./ui";
import { loadModels, pickVisionModel } from "./models";
import { cancelActivePull, runPull } from "./pull";

// Прикреплённый документ (Фаза A). text уже усечён под бюджет контекста
// (≈2 симв/токен: 8000 симв ≈ 4000 токенов — половина num_ctx, остаток под систему/вопрос/ответ).
const DOC_CHAR_BUDGET = 8000;

// Что можно приложить. Один список на три пути (кнопка, перетаскивание, вставка),
// чтобы фильтр диалога и проверка перетащенного файла не разъезжались; те же
// расширения принимает Rust (extract_document / read_image_base64).
const DOC_EXTS = ["pdf", "docx", "txt", "md"];
const IMAGE_EXTS = ["png", "jpg", "jpeg", "webp", "gif"];
const KINDS_HINT =
  "Подходят документы (PDF, Word, TXT, Markdown) и изображения (PNG, JPG, WEBP, GIF).";

// Имя и расширение из пути. Разделитель не хардкодим: путь приходит от системы,
// на Windows он с «\», на Linux/macOS с «/».
function fileNameOf(path: string): string {
  const parts = path.split(/[\\/]/);
  return parts[parts.length - 1] || path;
}

function extOf(path: string): string {
  const name = fileNameOf(path);
  const dot = name.lastIndexOf(".");
  return dot > 0 ? name.slice(dot + 1).toLowerCase() : "";
}

// Общая обёртка над нативным диалогом выбора файла: вернёт путь или null (отмена).
async function pickFile(label: string, extensions: string[]): Promise<string | null> {
  try {
    const sel = await open({ multiple: false, filters: [{ name: label, extensions }] });
    return typeof sel === "string" ? sel : null;
  } catch (e) {
    addError(`Не удалось открыть диалог: ${e}`);
    return null;
  }
}

// ── Документ ─────────────────────────────────────────────────────────────────

// Чип над полем ввода в режиме загрузки (пока Rust извлекает текст).
function showDocChipLoading() {
  docChipBadgeEl.className = "fmt-badge fmt--txt";
  docChipBadgeEl.textContent = "…";
  docChipNameEl.textContent = "Читаю документ…";
  docChipEl.classList.add("doc-chip--loading");
  docRemoveBtn.hidden = true;
  docChipEl.hidden = false;
}

// Чип над полем ввода для готового документа (бейдж формата + имя + кнопка «убрать»).
function showDocChip(doc: DocAttachment) {
  const fmt = fileFormat(doc.ext);
  docChipBadgeEl.className = `fmt-badge ${fmt.cls}`;
  docChipBadgeEl.textContent = fmt.label;
  docChipNameEl.textContent = doc.name;
  docChipEl.title = `${doc.name} · ${docSubline(doc)}`;
  docChipEl.classList.remove("doc-chip--loading");
  docRemoveBtn.hidden = false;
  docChipEl.hidden = false;
}

function hideDocChip() {
  docChipEl.hidden = true;
  docChipEl.classList.remove("doc-chip--loading");
  docChipEl.removeAttribute("title");
}

// Полностью сбросить «ожидающий» документ и убрать чип из композера.
export function clearPendingDoc() {
  state.pendingDoc = null;
  hideDocChip();
}

// «Прикрепить документ»: нативный диалог → загрузка по пути.
async function attachDocument() {
  const path = await pickFile("Документы", DOC_EXTS);
  if (!path) return; // отмена выбора
  await loadDocument(path);
}

// Загрузка документа по пути: извлечение текста в Rust → чип над полем ввода.
// Путь одинаков для кнопки и для перетаскивания файла в окно.
async function loadDocument(path: string) {
  attachBtn.disabled = true;
  showDocChipLoading();

  let doc: { name: string; ext: string; text: string; chars: number };
  try {
    doc = await invoke("extract_document", { path });
  } catch (e) {
    clearPendingDoc();
    attachBtn.disabled = false;
    addError(String(e));
    return;
  }

  // Бюджет контекста: большой документ не валим целиком — берём первую часть и предупреждаем.
  const truncated = doc.text.length > DOC_CHAR_BUDGET;
  const text = truncated ? doc.text.slice(0, DOC_CHAR_BUDGET) : doc.text;
  state.pendingDoc = { name: doc.name, ext: doc.ext, text, chars: doc.chars, truncated };
  showDocChip(state.pendingDoc);
  attachBtn.disabled = false;
  if (truncated) {
    addNotice(
      `Документ «${doc.name}» большой (${doc.chars.toLocaleString("ru")} символов). ` +
        `Использована первая часть (~${DOC_CHAR_BUDGET.toLocaleString("ru")} символов). ` +
        `Полная работа с большими документами появится в следующем этапе (поиск по документу).`,
    );
  }
  inputEl.focus();
}

function removeDocument() {
  clearPendingDoc();
  inputEl.focus();
}

// ── Изображение (зрение) ─────────────────────────────────────────────────────

function showImageChip(b64: string) {
  imgChipThumb.src = imageDataUrl(b64);
  imgChipEl.hidden = false;
}

export function clearPendingImage() {
  state.pendingImage = null;
  imgChipEl.hidden = true;
  imgChipThumb.removeAttribute("src");
}

// «Прикрепить изображение»: диалог → загрузка по пути.
async function attachImage() {
  const path = await pickFile("Изображения", IMAGE_EXTS);
  if (!path) return;
  await loadImage(path);
}

// Загрузка изображения по пути: чтение base64 в Rust (там же проверка формата,
// размера и перекодирование WEBP) → превью + гейт vision-модели.
async function loadImage(path: string) {
  let b64: string;
  try {
    b64 = await invoke<string>("read_image_base64", { path });
  } catch (e) {
    addError(String(e)); // формат/размер — понятная ошибка из Rust
    return;
  }
  setPendingImage(b64);
}

// Единая точка «картинка готова»: состояние, превью, гейт зрения, фокус в поле.
// Через неё проходят и файл с диска, и вставка из буфера — состояние заполняется одинаково.
function setPendingImage(b64: string) {
  state.pendingImage = b64;
  showImageChip(b64);
  ensureVisionModel(); // подобрать/переключить vision-модель или предложить установку
  inputEl.focus();
}

function removeImage() {
  clearPendingImage();
  inputEl.focus();
}

// Гейт зрения при прикреплении: проверяем, что картинку есть кому «увидеть».
// Сам ход с изображением уйдёт на модель зрения автоматически (chat.ts), а
// следующий обычный вопрос так же автоматически вернётся к текстовой модели —
// переключать ничего не нужно. Нет ни одной модели зрения — предлагаем установить.
export function ensureVisionModel() {
  if (visionByModel.get(state.selectedModel)) return; // текущая модель видит изображения
  if (pickVisionModel()) {
    addNotice("Изображение прикреплено — на вопросы с ним ответит модель зрения.");
  } else {
    offerInstallVision();
  }
}

// Нет модели зрения → предложить установить её через единый runPull.
// Ставим ЛЁГКИЙ вариант независимо от «светофора»: уровень green даётся уже за 16 ГБ ОЗУ
// (VRAM может быть всего 6 ГБ), а 8B на 6 ГБ видеопамяти — впритык (CLAUDE.md). Навязывать
// лишние 3 ГБ загрузки ради зрения не стоит. Старшую модель приложение всё равно возьмёт,
// если пользователь поставил её сам из настроек (см. bestOfRole в models.ts).
// Тег берём из набора (последний в лестнице роли = самый лёгкий), своего списка
// моделей здесь не держим: он разошёлся бы с MODEL_SET в Rust при первой же правке.
function visionInstallTag(): string {
  const ladder = autoTagsForRole("vision");
  return ladder[ladder.length - 1] ?? "";
}

async function offerInstallVision() {
  if (state.pullingTag) {
    addNotice(`Дождитесь завершения установки «${state.pullingTag}» — затем можно ставить модель зрения.`);
    return;
  }
  const VISION_MODEL = visionInstallTag();
  if (!VISION_MODEL) {
    // Набор ещё не получен от движка — предлагать конкретную модель не из чего.
    addNotice(
      "Не удалось определить, какую модель зрения предложить: движок не отвечает. " +
        "Нажмите кнопку обновления в шапке и попробуйте снова.",
    );
    return;
  }
  const ok = await confirmModal(
    `Для работы с изображениями нужна модель зрения. Установить ${VISION_MODEL} (~3–4 ГБ)? Потребуется интернет.`,
    "Установить",
  );
  if (!ok) {
    addNotice(
      "Чтобы работать с изображениями, установите модель зрения (qwen3-vl) — " +
        "онлайн или с диска: Настройки → «Модели».",
    );
    return;
  }
  const row = document.createElement("div");
  row.className = "notice";
  const label = document.createElement("span");
  label.textContent = `Установка ${VISION_MODEL}…`;
  const cancelBtn = document.createElement("button");
  cancelBtn.type = "button";
  cancelBtn.className = "notice-cancel";
  cancelBtn.textContent = "Отмена";
  cancelBtn.addEventListener("click", () => {
    cancelBtn.disabled = true;
    cancelActivePull();
  });
  row.append(label, cancelBtn);
  messagesEl.appendChild(row);
  refreshEmptyState();
  scrollToBottom();

  const outcome = await runPull(VISION_MODEL, {
    progress: (t) => {
      label.textContent = `Установка ${VISION_MODEL}: ${t}`;
      scrollToBottom();
    },
    done: () => {
      label.textContent = `Модель ${VISION_MODEL} установлена — можно работать с изображениями.`;
    },
    cancelled: () => {
      label.textContent = `Установка ${VISION_MODEL} отменена — можно вернуться к ней позже.`;
    },
    error: (m) => {
      row.className = "err";
      label.textContent = `Не удалось установить ${VISION_MODEL}: ${m}`;
    },
  });
  cancelBtn.remove();
  if (outcome === "done") {
    await loadModels();
    ensureVisionModel(); // теперь vision-модель есть → сообщим, что ответит она
  }
}

// ── Перетаскивание файла в окно ──────────────────────────────────────────────

// У окна Tauri включён СВОЙ перехват перетаскивания, поэтому обычные события
// dragover/drop до страницы не доходят — слушаем событие окна. Оно приносит
// готовые пути к файлам, то есть ровно то, с чем уже работают extract_document
// и read_image_base64: отдельная ветка загрузки не нужна.
let dragDropWired = false;

// Подсветка зоны броска — на самом поле ввода с кнопками.
function setDragOver(on: boolean) {
  const composer = composerWrapEl.querySelector(".composer");
  composer?.classList.toggle("composer--dragover", on);
}

async function wireDragDrop() {
  if (dragDropWired) return; // одна подписка на всё время работы окна
  dragDropWired = true;
  try {
    await getCurrentWebview().onDragDropEvent((e) => {
      const ev = e.payload;
      if (ev.type === "leave") {
        setDragOver(false);
        return;
      }
      // Открыты настройки, экран проекта или мастер — композера на экране нет,
      // принимать файл некуда (и сообщение в ленте было бы не видно).
      if (activeScreen() !== "chat") return;
      if (ev.type === "drop") {
        setDragOver(false);
        void acceptDroppedFiles(ev.paths);
      } else {
        setDragOver(true); // enter/over
      }
    });
  } catch {
    // Событий окна нет (страница открыта вне Tauri) — остаются кнопки прикрепления.
    dragDropWired = false;
  }
}

// Разбор брошенных файлов. Композер держит один документ и одну картинку, поэтому
// берём первый подходящий файл каждого вида, а про остальные честно говорим.
async function acceptDroppedFiles(paths: string[]) {
  const doc = paths.find((p) => DOC_EXTS.includes(extOf(p)));
  const image = paths.find((p) => IMAGE_EXTS.includes(extOf(p)));

  if (!doc && !image) {
    addError(
      paths.length === 1
        ? `Файл «${fileNameOf(paths[0])}» приложить нельзя. ${KINDS_HINT}`
        : `Ни один из перетащенных файлов приложить нельзя. ${KINDS_HINT}`,
    );
    return;
  }

  const skipped = paths.length - (doc ? 1 : 0) - (image ? 1 : 0);
  if (skipped > 0) {
    const taken: string[] = [];
    if (doc) taken.push(`документ «${fileNameOf(doc)}»`);
    if (image) taken.push(`изображение «${fileNameOf(image)}»`);
    const word = plural(skipped, "файл", "файла", "файлов");
    const verb = plural(skipped, "пропущен", "пропущены", "пропущены");
    addNotice(
      `Приложено: ${taken.join(" и ")}. Ещё ${skipped} ${word} ${verb} — ` +
        `за один раз можно приложить один документ и одно изображение.`,
    );
  }

  if (doc) await loadDocument(doc);
  if (image) await loadImage(image);
}

// ── Вставка изображения из буфера обмена ─────────────────────────────────────

// В буфере лежат БАЙТЫ картинки, а не путь к файлу, поэтому чтение через Rust здесь
// не подходит — base64 собираем на месте и кладём в то же состояние, что и файл.
const MAX_IMAGE_BYTES = 12 * 1024 * 1024; // как в read_image_base64: картинка ест контекст модели
// Движок читает PNG/JPEG/GIF как есть; всё прочее (в первую очередь WEBP) он не понимает —
// для файлов это чинит Rust, а вставленное из буфера перерисовываем в PNG здесь.
const ENGINE_IMAGE_TYPES = ["image/png", "image/jpeg", "image/gif"];
const MAX_IMAGE_SIDE = 2048; // модели зрения всё равно ужимают вход — не гоняем лишние байты

function wirePaste() {
  document.addEventListener("paste", (e) => {
    if (activeScreen() !== "chat") return;
    const img = clipboardImage(e.clipboardData);
    if (!img) return; // в буфере текст — вставка работает как обычно
    e.preventDefault();
    void attachPastedImage(img);
  });
}

// Картинка из буфера, если она там есть. Скриншот система нередко кладёт сразу в
// нескольких видах (картинка + текст) — берём первую именно картинку.
function clipboardImage(data: DataTransfer | null): File | null {
  if (!data) return null;
  for (let i = 0; i < data.items.length; i++) {
    const item = data.items[i];
    if (item.kind !== "file" || !item.type.startsWith("image/")) continue;
    const file = item.getAsFile();
    if (file) return file;
  }
  return null;
}

async function attachPastedImage(file: File) {
  let b64: string;
  try {
    const blob = await normalizeClipboardImage(file);
    if (blob.size > MAX_IMAGE_BYTES) {
      addError(
        `Слишком большое изображение (${Math.round(blob.size / 1024 / 1024)} МБ). ` +
          `Максимум — 12 МБ; уменьшите картинку.`,
      );
      return;
    }
    b64 = await blobToBase64(blob);
  } catch {
    addError(
      "Не удалось вставить изображение из буфера обмена. " +
        "Сохраните его в файл и приложите кнопкой с картинкой.",
    );
    return;
  }
  setPendingImage(b64);
}

// Привести вставленную картинку к формату, который движок точно прочитает.
// Знакомые форматы отдаём как есть (быстро и без потерь), остальные перерисовываем
// в PNG, заодно ужимая слишком большой кадр.
async function normalizeClipboardImage(file: Blob): Promise<Blob> {
  if (ENGINE_IMAGE_TYPES.includes(file.type)) return file;
  const bitmap = await createImageBitmap(file);
  const scale = Math.min(1, MAX_IMAGE_SIDE / Math.max(bitmap.width, bitmap.height));
  const canvas = document.createElement("canvas");
  canvas.width = Math.max(1, Math.round(bitmap.width * scale));
  canvas.height = Math.max(1, Math.round(bitmap.height * scale));
  const ctx = canvas.getContext("2d");
  if (!ctx) throw new Error("нет 2d-контекста");
  ctx.drawImage(bitmap, 0, 0, canvas.width, canvas.height);
  bitmap.close();
  const png = await new Promise<Blob | null>((resolve) => canvas.toBlob(resolve, "image/png"));
  if (!png) throw new Error("не удалось перекодировать изображение");
  return png;
}

// Байты → base64. Через FileReader: он не разворачивает картинку в строку символов,
// на многомегабайтном скриншоте это заметно безопаснее ручного btoa.
function blobToBase64(blob: Blob): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => {
      const url = String(reader.result);
      const comma = url.indexOf(",");
      if (comma < 0) reject(new Error("пустой результат чтения"));
      else resolve(url.slice(comma + 1)); // отрезаем «data:image/png;base64,»
    };
    reader.onerror = () => reject(reader.error ?? new Error("ошибка чтения"));
    reader.readAsDataURL(blob);
  });
}

// Обработчики кнопок прикрепления (OCR-кнопка живёт в chat.ts — это отправка).
export function wireAttachments() {
  attachBtn.addEventListener("click", attachDocument);
  docRemoveBtn.addEventListener("click", removeDocument);
  imageBtn.addEventListener("click", attachImage);
  imgRemoveBtn.addEventListener("click", removeImage);
  void wireDragDrop();
  wirePaste();
}
