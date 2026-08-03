// ── Старт приложения: изоляция модулей и видимый отчёт об отказе ─────────────
//
// Зачем это появилось. Раньше стартовая последовательность была одной длинной
// цепочкой: первое же исключение в любом `wire*` обрывало ВСЕ следующие, и человек
// видел окно без половины интерфейса — без единого намёка, где сломалось. Ровно так
// «пропадала» карточка установки модели распознавания: сама она была цела, а до неё
// просто не доходила очередь.
//
// Два правила, на которых держится этот модуль:
//
//  1. Отказ одного модуля не отменяет остальные. Приложение без карточки настроек
//     полезнее приложения без ничего.
//  2. Проглоченных исключений здесь НЕТ. Пойманное — это не «починенное»: оно
//     уходит в журнал и на экран. Интерфейс, который выглядит целым за счёт
//     съеденной ошибки, — худший из возможных исходов, потому что чинить его
//     начнут не раньше, чем он развалится у клиента.

import { invoke } from "@tauri-apps/api/core";

export interface BootStep {
  /// Человеческое имя: попадёт в журнал и на плашку, читать его будет не программист.
  name: string;
  run: () => void | Promise<void>;
}

export interface BootFailure {
  name: string;
  error: string;
}

/// Выполнить шаги старта, каждый в своей изоляции. Возвращает список упавших —
/// пустой список означает «всё поднялось», и это единственное, что он означает.
export async function runBoot(steps: BootStep[]): Promise<BootFailure[]> {
  const failures: BootFailure[] = [];
  for (const step of steps) {
    try {
      await step.run();
    } catch (e) {
      const error = describe(e);
      failures.push({ name: step.name, error });
      // Порядок важен: сначала записать, потом продолжать. Если следующий шаг
      // уронит окно совсем, запись уже будет на диске.
      report("error", "старт", `модуль «${step.name}» не запустился: ${error}`);
    }
  }
  return failures;
}

/// Плашка поверх окна со списком упавших модулей и путём к журналу.
///
/// Именно плашка, а не запись в консоль: у заказчика нет консоли разработчика и нет
/// привычки в неё ходить. Отказ, о котором приложение молчит, для него неотличим от
/// «программа кривая».
export async function showBootFailure(failures: BootFailure[]): Promise<void> {
  if (!failures.length) return;

  let logPath: string | null = null;
  try {
    logPath = await invoke<string | null>("journal_path");
  } catch {
    /* журнал недоступен — плашку всё равно показываем */
  }

  const box = document.createElement("div");
  box.className = "boot-error";
  box.setAttribute("role", "alert");

  const title = document.createElement("div");
  title.className = "boot-error__title";
  title.textContent =
    failures.length === 1
      ? "Одна часть приложения не запустилась"
      : `${failures.length} частей приложения не запустились`;
  box.appendChild(title);

  const list = document.createElement("ul");
  list.className = "boot-error__list";
  for (const f of failures) {
    const li = document.createElement("li");
    const strong = document.createElement("b");
    strong.textContent = f.name;
    li.appendChild(strong);
    li.appendChild(document.createTextNode(` — ${f.error}`));
    list.appendChild(li);
  }
  box.appendChild(list);

  const hint = document.createElement("div");
  hint.className = "boot-error__hint";
  hint.textContent = logPath
    ? `Остальное работает. Подробности — в журнале: ${logPath}`
    : "Остальное работает. Подробности — в настройках, на экране «Проверка системы».";
  box.appendChild(hint);

  const close = document.createElement("button");
  close.type = "button";
  close.className = "boot-error__close";
  close.textContent = "Скрыть";
  close.addEventListener("click", () => box.remove());
  box.appendChild(close);

  document.body.appendChild(box);
}

/// Перехват ошибок окна: всё, что всплыло мимо наших try/catch, тоже попадает в
/// журнал. Без этого ошибка в обработчике клика видна только в консоли, которой у
/// пользователя нет, — а именно такие ошибки и приходят потом как «ничего не работает».
export function installErrorReporting(): void {
  window.addEventListener("error", (e) => {
    const where = e.filename ? ` (${e.filename}:${e.lineno})` : "";
    report("error", "окно", `${e.message}${where}`);
  });
  window.addEventListener("unhandledrejection", (e) => {
    report("error", "окно", `необработанный отказ: ${describe(e.reason)}`);
  });
}

/// Запись в журнал приложения. Сама по себе она упасть не должна ни при каких
/// обстоятельствах: падение из-за неудачной попытки пожаловаться — это издевательство.
export function report(level: "info" | "warn" | "error", source: string, message: string): void {
  try {
    void invoke("journal_log", { level, source, message }).catch(() => {});
  } catch {
    /* моста в Rust нет (тесты, сборка без бэкенда) — переживём */
  }
}

/// Человеческое описание пойманного: Error, строка, объект ошибки IPC { code, message }.
function describe(e: unknown): string {
  if (e instanceof Error) return e.message || e.name;
  if (typeof e === "string") return e;
  if (e && typeof e === "object" && "message" in e) {
    const m = (e as { message: unknown }).message;
    if (typeof m === "string") return m;
  }
  try {
    return JSON.stringify(e);
  } catch {
    return String(e);
  }
}
