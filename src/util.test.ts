// Разбор технических ошибок в человеческий текст: ошибиться здесь дорого —
// пользователь-непрограммер пойдёт чинить не то, что сломалось.
import { describe, expect, it } from "vitest";
import { humanError, plural } from "./util";

describe("humanError: сообщение без кода", () => {
  it("показывает строку как есть и НЕ угадывает причину по тексту", () => {
    // Ровно те строки, на которых прежний разбор давал подсказку. Теперь команды,
    // не обращающиеся к движку, не присылают кода — и выдумывать причину нельзя:
    // «connection refused» может прийти из чего угодно, а не только от движка.
    for (const s of [
      "error sending request: tcp connect error: Connection refused",
      "operation timed out",
      'model "qwen3.5:9b" not found, try pulling it first',
      "model requires more system memory (9.2 GiB) than is available",
    ]) {
      expect(humanError(s)).toBe(s);
    }
  });

  it("не срабатывает на словах, похожих на маркеры", () => {
    // Прежний шаблон /tim/ ловил «runtime», /404/ — любой путь с этими цифрами.
    expect(humanError("panic in runtime: index out of bounds")).toBe(
      "panic in runtime: index out of bounds",
    );
    expect(humanError("Не удалось прочитать /home/u/404/файл.txt")).toBe(
      "Не удалось прочитать /home/u/404/файл.txt",
    );
  });

  it("длинную строку режет, не разрубая символы", () => {
    const out = humanError("😀".repeat(300));
    expect(out.endsWith("…")).toBe(true);
    // Одиночный суррогат на срезе не пережил бы сериализацию IPC — строка обязана
    // остаться корректной (каждая суррогатная пара целой).
    const loneSurrogate = /[\uD800-\uDBFF](?![\uDC00-\uDFFF])|(?:[^\uD800-\uDBFF]|^)[\uDC00-\uDFFF]/;
    expect(loneSurrogate.test(out)).toBe(false);
    expect([...out].length).toBe(201); // 200 кодовых точек + многоточие
  });
});

describe("plural", () => {
  it("склоняет по русским правилам", () => {
    expect(plural(1, "ядро", "ядра", "ядер")).toBe("ядро");
    expect(plural(4, "ядро", "ядра", "ядер")).toBe("ядра");
    expect(plural(18, "ядро", "ядра", "ядер")).toBe("ядер");
    expect(plural(21, "ядро", "ядра", "ядер")).toBe("ядро");
    expect(plural(0, "ядро", "ядра", "ядер")).toBe("ядер");
  });
});

describe("humanError: ошибка с кодом причины из Rust", () => {
  it("узнаёт причину по коду, не заглядывая в текст", () => {
    // Текст намеренно не содержит ни одного маркера, по которому сработал бы
    // разбор строк, — значит сработал именно код.
    expect(humanError({ code: "engine_down", message: "какой угодно текст" })).toContain(
      "Движок не отвечает",
    );
    expect(humanError({ code: "timeout", message: "…" })).toContain("время ожидания");
    expect(humanError({ code: "model_missing", message: "…" })).toContain(
      "Модель не установлена",
    );
    expect(humanError({ code: "out_of_memory", message: "…" })).toContain("Не хватает памяти");
  });

  it("код важнее текста: текст про «refused» не перебивает код таймаута", () => {
    const out = humanError({ code: "timeout", message: "connection refused" });
    expect(out).toContain("время ожидания");
    expect(out).not.toContain("Движок не отвечает");
  });

  it("незнакомый код показывает сообщение из Rust, а не «неизвестная ошибка»", () => {
    // Бэкенд может оказаться новее интерфейса — сообщение уже человеческое.
    expect(humanError({ code: "disk_full", message: "На диске нет места." })).toBe(
      "На диске нет места.",
    );
  });

  it("код unknown означает «причина не распознана» — показываем текст как есть", () => {
    expect(humanError({ code: "unknown", message: "Странный сбой." })).toBe("Странный сбой.");
  });

  it("объект без кода не превращается в [object Object]", () => {
    // Сообщение обязано дойти до пользователя, даже если формы ошибки мы не знаем.
    expect(humanError({ message: "Не удалось записать файл." })).toBe(
      "Не удалось записать файл.",
    );
    expect(humanError(new Error("Сбой разбора настроек."))).toBe("Сбой разбора настроек.");
  });
});
