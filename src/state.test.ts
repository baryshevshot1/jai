// Набор моделей приходит из Rust (MODEL_SET) и на фронтенде не дублируется.
// Эти тесты закрепляют правила, по которым из него строится авто-подбор: порядок
// «старшая → лёгкая» и исключение профилей под ручной выбор. Разъедется — упадёт
// здесь, а не у клиента в виде «приложение предлагает не ту модель».
import { beforeEach, describe, expect, it } from "vitest";
import { autoTagsForRole, modelSet, tagForRole } from "./state";

// Слепок реального набора: роли и веса те же, что в MODEL_SET.
function loadSet() {
  modelSet.clear();
  const rows: [string, string, number, boolean][] = [
    ["qwen3.5:9b", "text", 6.6, true],
    ["qwen3.5:4b", "text", 3.0, true],
    ["hf.co/t-tech/T-lite-it-2.1-GGUF:Q4_K_M", "text", 5.1, false],
    ["bge-m3:latest", "embed", 1.2, false],
    ["qwen3-vl:8b", "vision", 6.5, true],
    ["qwen3-vl:4b", "vision", 3.5, true],
    ["qwen3-coder:latest", "code", 19.0, false],
  ];
  for (const [tag, role, approxGb, autoPick] of rows) {
    modelSet.set(tag, { role, approxGb, autoPick });
  }
}

describe("autoTagsForRole", () => {
  beforeEach(loadSet);

  it("выстраивает модели роли от старшей к лёгкой", () => {
    expect(autoTagsForRole("text")).toEqual(["qwen3.5:9b", "qwen3.5:4b"]);
    expect(autoTagsForRole("vision")).toEqual(["qwen3-vl:8b", "qwen3-vl:4b"]);
  });

  it("не берёт в авто-подбор профиль под ручной выбор", () => {
    // Русский профиль T-lite тяжелее лёгкой текстовой (5.1 против 3.0) и по одному
    // лишь весу встал бы в середину лестницы — но назначать его сам продукт не должен.
    expect(autoTagsForRole("text")).not.toContain("hf.co/t-tech/T-lite-it-2.1-GGUF:Q4_K_M");
  });

  it("не путает роли между собой", () => {
    expect(autoTagsForRole("text")).not.toContain("qwen3-vl:8b");
    expect(autoTagsForRole("vision")).not.toContain("qwen3.5:9b");
    expect(autoTagsForRole("embed")).toEqual([]); // эмбеддинги не подбираются по железу
    expect(autoTagsForRole("code")).toEqual([]);
  });

  it("на пустом наборе отдаёт пустую лестницу, а не выдумывает теги", () => {
    modelSet.clear();
    expect(autoTagsForRole("text")).toEqual([]);
    expect(autoTagsForRole("vision")).toEqual([]);
  });

  it("самый лёгкий в лестнице зрения — то, что предлагается к установке", () => {
    const ladder = autoTagsForRole("vision");
    expect(ladder[ladder.length - 1]).toBe("qwen3-vl:4b");
  });
});

describe("tagForRole", () => {
  beforeEach(loadSet);

  it("находит модель поиска по документам", () => {
    expect(tagForRole("embed")).toBe("bge-m3:latest");
  });

  it("на пустом наборе возвращает пустую строку", () => {
    modelSet.clear();
    expect(tagForRole("embed")).toBe("");
  });
});
