// Прикрепления в композере: документ (Фаза A) и изображение (зрение, qwen3-vl),
// подбор/установка vision-модели.

import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import type { DocAttachment } from "./types";
import { state, visionByModel } from "./state";
import {
  attachBtn,
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
import { docSubline, fileFormat, imageDataUrl } from "./util";
import {
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

// «Прикрепить документ»: нативный диалог → извлечение текста в Rust → чип.
async function attachDocument() {
  let path: string | null;
  try {
    const sel = await open({
      multiple: false,
      filters: [{ name: "Документы", extensions: ["pdf", "docx", "txt", "md"] }],
    });
    path = typeof sel === "string" ? sel : null;
  } catch (e) {
    addError(`Не удалось открыть диалог: ${e}`);
    return;
  }
  if (!path) return; // отмена выбора

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

// «Прикрепить изображение»: диалог → чтение base64 в Rust → превью + гейт vision-модели.
async function attachImage() {
  let path: string | null;
  try {
    const sel = await open({
      multiple: false,
      filters: [{ name: "Изображения", extensions: ["png", "jpg", "jpeg", "webp", "gif"] }],
    });
    path = typeof sel === "string" ? sel : null;
  } catch (e) {
    addError(`Не удалось открыть диалог: ${e}`);
    return;
  }
  if (!path) return;

  let b64: string;
  try {
    b64 = await invoke<string>("read_image_base64", { path });
  } catch (e) {
    addError(String(e)); // формат/размер — понятная ошибка из Rust
    return;
  }
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

// Нет модели зрения → предложить установить qwen3-vl через единый runPull.
// Ставим ЛЁГКИЙ вариант независимо от «светофора»: уровень green даётся уже за 16 ГБ ОЗУ
// (VRAM может быть всего 6 ГБ), а 8B на 6 ГБ видеопамяти — впритык (CLAUDE.md). Навязывать
// лишние 3 ГБ загрузки ради зрения не стоит. Старшую модель приложение всё равно возьмёт,
// если пользователь поставил её сам из настроек (см. bestOfRole в models.ts).
const VISION_MODEL = "qwen3-vl:4b";

async function offerInstallVision() {
  if (state.pullingTag) {
    addNotice(`Дождитесь завершения установки «${state.pullingTag}» — затем можно ставить модель зрения.`);
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

// Обработчики кнопок прикрепления (OCR-кнопка живёт в chat.ts — это отправка).
export function wireAttachments() {
  attachBtn.addEventListener("click", attachDocument);
  docRemoveBtn.addEventListener("click", removeDocument);
  imageBtn.addEventListener("click", attachImage);
  imgRemoveBtn.addEventListener("click", removeImage);
}
