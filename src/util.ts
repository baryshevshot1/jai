// Мелкие чистые помощники, нужные многим модулям: форматирование, склонения,
// человеческие тексты ошибок, общие SVG-иконки. Ни от чего не зависят.

import type { DocAttachment } from "./types";

// Сырые технические ошибки (reqwest/serde/Ollama) → короткий человеческий текст.
// Детали остаются в console для диагностики; пользователю — понятная суть.
export function humanError(e: unknown): string {
  const s = String(e);
  console.error(s);
  if (/connect|Connection|11434|отказано|refused/i.test(s))
    return "Движок Ollama недоступен. Проверьте, запущен ли он (кнопка «Проверка»).";
  if (/tim| timed out|timeout/i.test(s)) return "Превышено время ожидания ответа движка.";
  if (/no such|not found|404/i.test(s))
    return "Модель не установлена. Откройте «Настройки → Модели».";
  if (/memory|OOM|allocat/i.test(s))
    return "Не хватает памяти для модели. Выберите модель полегче или меньший контекст.";
  return s.length > 200 ? s.slice(0, 200) + "…" : s;
}

// Склонение существительного по числу (русские правила): 1 ядро, 4 ядра, 18 ядер.
export function plural(n: number, one: string, few: string, many: string): string {
  const m10 = n % 10;
  const m100 = n % 100;
  if (m10 === 1 && m100 !== 11) return one;
  if (m10 >= 2 && m10 <= 4 && (m100 < 12 || m100 > 14)) return few;
  return many;
}

// Байты → «N.N ГБ» (единый форматтер объёмов моделей/загрузок).
export function gb(bytes: number): string {
  return `${(bytes / 1024 / 1024 / 1024).toFixed(1)} ГБ`;
}

// Частые статусы Ollama /api/pull → понятный русский.
export function ruPullStatus(status: string): string {
  if (status.startsWith("pulling manifest")) return "Получение манифеста";
  if (status.startsWith("pulling")) return "Скачивание";
  if (status.startsWith("verifying")) return "Проверка";
  if (status.startsWith("writing")) return "Запись";
  if (status.startsWith("removing")) return "Очистка";
  if (status === "success") return "Готово";
  return status || "Установка";
}

// Формат файла → подпись бейджа и CSS-класс цвета. Неизвестное — нейтральный «ФАЙЛ».
export function fileFormat(ext: string): { label: string; cls: string } {
  switch (ext.toLowerCase()) {
    case "pdf":
      return { label: "PDF", cls: "fmt--pdf" };
    case "docx":
    case "doc":
      return { label: "DOCX", cls: "fmt--docx" };
    case "md":
      return { label: "MD", cls: "fmt--md" };
    case "txt":
      return { label: "TXT", cls: "fmt--txt" };
    default:
      return { label: "ФАЙЛ", cls: "fmt--txt" };
  }
}

// Подпись под именем файла: полный размер либо отметка усечённого фрагмента.
export function docSubline(doc: DocAttachment): string {
  const n = doc.chars.toLocaleString("ru");
  const unit = plural(doc.chars, "символ", "символа", "символов");
  return doc.truncated ? `Фрагмент · из ${n} ${unit}` : `${n} ${unit}`;
}

// MIME изображения по сигнатуре base64 (для data-URL превью; точный тип не храним).
export function imageMime(b64: string): string {
  if (b64.startsWith("/9j/")) return "image/jpeg";
  if (b64.startsWith("iVBORw0KG")) return "image/png";
  if (b64.startsWith("UklGR")) return "image/webp";
  if (b64.startsWith("R0lGOD")) return "image/gif";
  return "image/png";
}

export function imageDataUrl(b64: string): string {
  return `data:${imageMime(b64)};base64,${b64}`;
}

// Делает div-«кнопку» доступной с клавиатуры: Tab-фокус, роль и активация
// Enter/Space (обработчик клика навешивает вызывающий код). Для строк списков
// (чаты, проекты), которые исторически не <button> из-за вложенных кнопок «×».
export function makePressable(el: HTMLElement) {
  el.tabIndex = 0;
  el.setAttribute("role", "button");
  el.addEventListener("keydown", (e) => {
    if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      el.click();
    }
  });
}

// Общие контурные иконки (список моделей, кнопки проверок, диагностика, обновления).
export const ICON_DOWNLOAD = '<svg viewBox="0 0 24 24"><path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4M7 10l5 5 5-5M12 15V3"/></svg>';
export const ICON_REFRESH_CW = '<svg viewBox="0 0 24 24"><path d="M3 12a9 9 0 0 1 15-6.7L21 8M21 3v5h-5M21 12a9 9 0 0 1-15 6.7L3 16M3 21v-5h5"/></svg>';
export const ICON_CHECK = '<svg viewBox="0 0 24 24"><path d="M20 6 9 17l-5-5"/></svg>';
export const ICON_ALERT = '<svg viewBox="0 0 24 24"><path d="M10.3 3.9 1.8 18a2 2 0 0 0 1.7 3h16.9a2 2 0 0 0 1.7-3L13.7 3.9a2 2 0 0 0-3.4 0z"/><path d="M12 9v4M12 17h.01"/></svg>';
