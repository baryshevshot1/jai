// Разбор технических ошибок в человеческий текст: ошибиться здесь дорого —
// пользователь-непрограммер пойдёт чинить не то, что сломалось.
import { describe, expect, it } from "vitest";
import { humanError, plural } from "./util";

describe("humanError", () => {
  it("узнаёт недоступный движок", () => {
    expect(humanError("error sending request: tcp connect error: Connection refused")).toContain(
      "Движок не отвечает",
    );
  });

  it("узнаёт таймаут", () => {
    expect(humanError("operation timed out")).toContain("время ожидания");
  });

  it("НЕ принимает «runtime» за таймаут", () => {
    // Прежний шаблон /tim/ ловил любое слово с «tim» — «runtime», «estimate».
    const out = humanError("panic in runtime: index out of bounds");
    expect(out).not.toContain("время ожидания");
  });

  it("НЕ принимает «estimate» за таймаут", () => {
    expect(humanError("estimate failed")).not.toContain("время ожидания");
  });

  it("узнаёт отсутствующую модель", () => {
    expect(humanError('model "qwen3.5:9b" not found, try pulling it first')).toContain(
      "Модель не установлена",
    );
  });

  it("НЕ принимает «no such file» за отсутствующую модель", () => {
    // Файловая ошибка — это не про установку моделей: подсказка увела бы не туда.
    const out = humanError("no such file or directory: /home/u/.config/jai/settings.json");
    expect(out).not.toContain("Модель не установлена");
  });

  it("узнаёт нехватку памяти", () => {
    expect(humanError("model requires more system memory (9.2 GiB) than is available")).toContain(
      "Не хватает памяти",
    );
  });

  it("нераспознанное отдаёт как есть", () => {
    expect(humanError("совершенно новая ошибка")).toBe("совершенно новая ошибка");
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
