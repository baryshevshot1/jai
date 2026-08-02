// Мелкие чистые помощники, нужные многим модулям: форматирование, склонения,
// человеческие тексты ошибок, общие SVG-иконки. Ни от чего не зависят.

import type { DocAttachment } from "./types";

// Технические ошибки → короткий человеческий текст. Детали остаются в console для
// диагностики; пользователю — понятная суть.
//
// Ошибка из Rust приходит в одном из двух видов:
//   { code, message } — причину назвал сам бэкенд в точке отказа (см. error.rs);
//   строка           — участок ещё не переведён на коды.
// Первый вид разбираем по коду, второй — по устойчивым маркерам текста. Разбор
// текста остаётся временно и должен исчезнуть по мере перевода команд: угадывание
// по формулировке ломается от любой правки текста в Rust.

// Тексты для кодов причин. Ключи — ровно значения ErrorCode в src-tauri/src/error.rs.
const BY_CODE: Record<string, string> = {
  engine_down:
    "Движок не отвечает. Нажмите кнопку обновления в шапке или откройте Настройки → «Проверка системы».",
  timeout: "Превышено время ожидания ответа движка.",
  model_missing: "Модель не установлена. Откройте Настройки → «Модели».",
  out_of_memory:
    "Не хватает памяти для модели. Закройте тяжёлые программы или установите модель полегче: Настройки → «Модели».",
};

// Форму проверяем, а не доверяем типу: на проводе IPC может оказаться что угодно —
// строка, объект с кодом, объект без кода, Error, вообще произвольное значение.
function errorCode(e: unknown): string | null {
  if (typeof e !== "object" || e === null) return null;
  const code = (e as Record<string, unknown>).code;
  return typeof code === "string" && code ? code : null;
}

// Текст ошибки в любом её виде. Без этого объект без кода превратился бы в
// «[object Object]», и сообщение потерялось бы целиком.
function errorText(e: unknown): string {
  if (typeof e === "object" && e !== null) {
    const message = (e as Record<string, unknown>).message;
    if (typeof message === "string" && message) return message;
  }
  return String(e);
}

const KNOWN_ERRORS: ReadonlyArray<[RegExp, string]> = [
  // Движок не запущен или порт закрыт: «connection refused», tcp connect error…
  [
    /connect|11434|refused|отказано/i,
    "Движок не отвечает. Нажмите кнопку обновления в шапке или откройте Настройки → «Проверка системы».",
  ],
  // Именно «timed out»/«timeout», а не любое слово с «tim» (runtime, estimate…).
  [/timed?\s?out|timeout|время ожидания/i, "Превышено время ожидания ответа движка."],
  // Ollama об отсутствующей модели: «model "…" not found, try pulling it first», HTTP 404.
  // Голое «not found» не считаем: файловое «no such file» — не про модель.
  [
    /model\b.{0,80}not found|try pulling|\b404\b/i,
    "Модель не установлена. Откройте Настройки → «Модели».",
  ],
  // Нехватка памяти при загрузке модели: ggml/CUDA-аллокации и «model requires
  // more system memory» от Ollama.
  [
    /out of memory|not enough memory|requires more.{0,30}memory|failed to allocate|cudamalloc|\boom\b/i,
    "Не хватает памяти для модели. Закройте тяжёлые программы или установите модель полегче: Настройки → «Модели».",
  ],
];

export function humanError(e: unknown): string {
  const code = errorCode(e);
  const text = errorText(e);
  console.error(code ? `[${code}] ${text}` : text);

  // Причину назвал бэкенд — верим ему и текст не разбираем.
  if (code && BY_CODE[code]) return BY_CODE[code];

  // Кода нет вовсе либо он "unknown" (участок ещё не переведён и честно говорит
  // «не знаю») — остаётся временный разбор текста по устойчивым маркерам.
  // Код, которого нет в таблице, — бэкенд новее интерфейса: его сообщение уже
  // человеческое и на русском, показываем как есть.
  if (!code || code === "unknown") {
    for (const [re, human] of KNOWN_ERRORS) if (re.test(text)) return human;
  }
  return clip(text);
}

// Обрезаем по кодовым точкам: slice по UTF-16-единицам мог бы разрубить эмодзи.
function clip(s: string): string {
  const cps = [...s];
  return cps.length > 200 ? cps.slice(0, 200).join("") + "…" : s;
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
    // Только когда сфокусирована сама строка: нажатие на вложенной кнопке («×»
    // удаления) всплывает сюда, и перехват отменил бы её нативную активацию —
    // с клавиатуры вместо удаления открывался бы диалог.
    if (e.target !== el) return;
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
