// Проверка, что приложение вообще СТАРТУЕТ.
//
// Это тот класс отказов, который не ловит ни компилятор, ни остальные тесты: код
// типизирован, модули собираются, а при запуске падает первая же функция — и
// пользователь видит окно без половины интерфейса.
//
// ЧЕСТНОЕ ПРЕДУПРЕЖДЕНИЕ О ГРАНИЦАХ ЭТОГО ТЕСТА. Первая его версия была зелёной при
// неработающем приложении: она подменяла мост в Rust заглушкой, которая ВСЕГДА
// отвечает успехом. В такой песочнице отказ моста — а именно им проявляется и
// незарегистрированная команда, и сборка без нужной фичи — просто непредставим.
// Поэтому здесь мост умеет и отказывать (`rejecting`), и это отдельные проверки:
// старт обязан переживать бэкенд, который говорит «нет» на всё.
//
// Чего этот тест по-прежнему НЕ проверяет и проверить не может: что бинарник
// действительно собран, что команды в нём зарегистрированы, что окно нарисовалось.
// Это закрывают `cargo test` (контракт команд) и smoke-проверка собранного бандла
// (`jai --self-check`), а не jsdom.
// @vitest-environment jsdom
import { beforeEach, describe, expect, it, vi } from "vitest";
// Разметку берём через сборщик (?raw), а не через файловый API: типы Node в проекте
// не подключены намеренно — код работает в браузерном движке, и соблазн вызвать
// файловую систему из интерфейса должен отсутствовать даже в тестах.
import indexHtml from "../index.html?raw";

// Мост в Rust: по умолчанию отвечает пустыми значениями, но умеет и отказывать —
// без этого режима тест не воспроизводит реальный отказ (см. шапку файла).
let rejecting = false;
const answers: Record<string, unknown> = {
  voice_available: false,
  voice_model_state: { installed: false, partial_bytes: 0, total_bytes: 487601967 },
  list_models: [],
  model_states: { models: [], others: [] },
  list_conversations: [],
  list_projects: [],
  list_documents: [],
  documents_empty: true,
  embedding_status: false,
  get_setting: null,
  journal_path: null,
};
vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(async (cmd: string) => {
    // journal_log обязан работать всегда: это путь, по которому уходит сама жалоба.
    if (rejecting && cmd !== "journal_log") {
      throw { code: "unknown", message: `команда ${cmd} не зарегистрирована` };
    }
    return cmd in answers ? answers[cmd] : null;
  }),
  Channel: class {
    onmessage: unknown = null;
  },
}));
vi.mock("@tauri-apps/plugin-dialog", () => ({ open: vi.fn(async () => null) }));
vi.mock("@tauri-apps/plugin-opener", () => ({ openUrl: vi.fn(async () => {}) }));
vi.mock("@tauri-apps/plugin-process", () => ({ relaunch: vi.fn(async () => {}) }));
vi.mock("@tauri-apps/plugin-updater", () => ({ check: vi.fn(async () => null) }));
vi.mock("@tauri-apps/api/app", () => ({ getVersion: vi.fn(async () => "0.0.0-test") }));
vi.mock("@tauri-apps/api/webview", () => ({
  getCurrentWebview: () => ({ onDragDropEvent: vi.fn(async () => () => {}) }),
}));

function loadMarkup() {
  const body = indexHtml.slice(indexHtml.indexOf("<body>") + 6, indexHtml.indexOf("</body>"));
  document.body.innerHTML = body;
}

const flush = () => new Promise((r) => setTimeout(r, 0));

describe("старт приложения", () => {
  beforeEach(() => {
    loadMarkup();
    vi.resetModules();
    rejecting = false;
  });

  it("реестр элементов заполняется по реальной разметке", async () => {
    const dom = await import("./dom");
    expect(() => dom.initDom()).not.toThrow();
    // Пара опорных элементов: если разметка и реестр разошлись, здесь будет null.
    expect(dom.messagesEl).toBeTruthy();
    expect(dom.inputEl).toBeTruthy();
  });

  it("все wire-функции отрабатывают без исключений", async () => {
    const dom = await import("./dom");
    dom.initDom();

    // Тот же порядок, что в main.ts: сначала реестр, потом обработчики.
    const mods = await Promise.all([
      import("./chat"),
      import("./attachments"),
      import("./documents"),
      import("./models"),
      import("./conversations"),
      import("./projects"),
      import("./settings"),
      import("./wizard"),
    ]);
    const [chat, attachments, documents, models, conversations, projects, settings, wizard] = mods;

    expect(() => chat.wireChat()).not.toThrow();
    expect(() => attachments.wireAttachments()).not.toThrow();
    expect(() => documents.initDocContexts()).not.toThrow();
    expect(() => documents.wireDocuments()).not.toThrow();
    expect(() => models.wireModels()).not.toThrow();
    expect(() => conversations.wireConversations()).not.toThrow();
    expect(() => projects.wireProjects()).not.toThrow();
    expect(() => settings.wireSettings()).not.toThrow();
    expect(() => wizard.wireWizard()).not.toThrow();
  });

  it("карточка установки модели распознавания появляется в настройках", async () => {
    const dom = await import("./dom");
    dom.initDom();
    const settings = await import("./settings");
    settings.wireSettings();

    // Без этой карточки поставить модель распознавания неоткуда, а значит и кнопки
    // микрофона пользователь никогда не увидит — функция есть только на бумаге.
    expect(document.querySelector("#voice-model-install"), "карточка не построена").toBeTruthy();
    expect(document.querySelector("#voice-model-restart"), "нет «Начать заново»").toBeTruthy();
    expect(document.querySelector("#voice-model-import"), "нет установки с флешки").toBeTruthy();
  });

  // ── Отказ бэкенда: то, что прежняя версия теста представить не могла ────────

  it("карточка строится даже когда бэкенд отказывает на все команды", async () => {
    rejecting = true;
    const dom = await import("./dom");
    dom.initDom();
    const settings = await import("./settings");

    // Именно так выглядит сборка без голосового ввода или без регистрации команд:
    // мост отвечает отказом. Карточка обязана появиться и честно сказать, что
    // модель не установлена, — а не исчезнуть вместе со всей страницей настроек.
    expect(() => settings.wireSettings()).not.toThrow();
    await flush();
    expect(document.querySelector("#voice-model-install")).toBeTruthy();
  });

  it("инициализация голосового ввода не роняет старт при отказе бэкенда", async () => {
    rejecting = true;
    const dom = await import("./dom");
    dom.initDom();
    const voice = await import("./voice");
    await expect(voice.initVoice()).resolves.not.toThrow();
    // Спросить было не у кого — кнопка остаётся скрытой, а не мёртвой.
    expect(dom.voiceBtn.hidden).toBe(true);
  });

  it("инициализация голосового ввода не роняет старт без модели", async () => {
    const dom = await import("./dom");
    dom.initDom();
    const voice = await import("./voice");
    await expect(voice.initVoice()).resolves.not.toThrow();
    expect(dom.voiceBtn.hidden).toBe(true);
  });
});

// ── Изоляция модулей старта ──────────────────────────────────────────────────
// Отказ, ради которого всё это писалось: одно исключение в середине цепочки
// отменяло ВСЕ следующие модули. До изоляции такой тест был бы красным.
describe("изоляция инициализации", () => {
  beforeEach(() => {
    loadMarkup();
    vi.resetModules();
    rejecting = false;
    document.querySelectorAll(".boot-error").forEach((n) => n.remove());
  });

  it("падение одного модуля не отменяет следующие", async () => {
    const { runBoot } = await import("./boot");
    const done: string[] = [];
    const failures = await runBoot([
      { name: "первый", run: () => void done.push("первый") },
      {
        name: "сломанный",
        run: () => {
          throw new Error("тут всё плохо");
        },
      },
      { name: "третий", run: () => void done.push("третий") },
    ]);

    expect(done, "модуль после упавшего не выполнился").toEqual(["первый", "третий"]);
    expect(failures).toHaveLength(1);
    expect(failures[0].name).toBe("сломанный");
    expect(failures[0].error).toContain("тут всё плохо");
  });

  it("асинхронный отказ модуля тоже попадает в отчёт", async () => {
    const { runBoot } = await import("./boot");
    const failures = await runBoot([
      { name: "медленный", run: async () => Promise.reject({ code: "unknown", message: "отказ" }) },
    ]);
    expect(failures).toHaveLength(1);
    expect(failures[0].error).toBe("отказ");
  });

  it("отказ виден на экране, а не только в консоли", async () => {
    const { showBootFailure } = await import("./boot");
    await showBootFailure([{ name: "настройки", error: "нет элемента" }]);

    const box = document.querySelector(".boot-error");
    expect(box, "плашка об отказе не показана").toBeTruthy();
    expect(box!.textContent).toContain("настройки");
    expect(box!.textContent).toContain("нет элемента");
  });

  it("успешный старт не рисует плашку", async () => {
    const { showBootFailure } = await import("./boot");
    await showBootFailure([]);
    expect(document.querySelector(".boot-error")).toBeNull();
  });
});

// ── Строка композера ─────────────────────────────────────────────────────────
// Регрессия вёрстки: пока переключатели режимов стояли в одном ряду с действиями,
// длинная подпись диктовки раздвигала ряд и уводила кнопку отправки вправо.
// Лечится разделением на две строки — и это надо удержать: вернуть кнопки обратно
// «чтобы компактнее» легко, а сломается при этом не они, а отправка.
describe("строка композера", () => {
  beforeEach(() => {
    loadMarkup();
    vi.resetModules();
  });

  it("все кнопки — в одном ряду, режимы сразу за микрофоном", () => {
    const bar = document.querySelector(".composer-bar")!;
    expect(bar, "нет ряда действий").toBeTruthy();

    const ids = [...bar.querySelectorAll("button")].map((b) => b.id).filter(Boolean);
    expect(ids, "порядок кнопок в ряду изменился").toEqual([
      "attach-btn",
      "image-btn",
      "voice-btn",
      "think-toggle",
      "online-toggle",
      "send-btn",
      "stop-btn",
    ]);
  });

  // Ровно это и ломало ряд: подпись диктовки стояла среди кнопок и распирала его,
  // уводя кнопку отправки. Вернуть её сюда — значит вернуть тот же дефект.
  it("подпись диктовки вынесена из ряда и обрезается внутри себя", () => {
    const state = document.querySelector("#voice-state")!;
    expect(
      state.closest(".composer-bar"),
      "подпись вернулась в ряд кнопок — она снова будет его распирать",
    ).toBeNull();
    expect(
      state.closest(".composer"),
      "подпись должна жить внутри карточки композера — она её точка отсчёта",
    ).toBeTruthy();
    expect(
      state.querySelector(".voice-state__text"),
      "текст подписи должен жить во вложенном span — только он умеет многоточие",
    ).toBeTruthy();
  });
});

describe("состояние, которое видно не только глазами", () => {
  beforeEach(() => {
    loadMarkup();
    vi.resetModules();
    rejecting = false;
  });

  // Незрячий пользователь слышал «Онлайн-режим, кнопка» и при включённом, и при
  // выключенном режиме — то есть не мог установить, уходят ли данные его запроса
  // наружу. Для продукта, обещающего 152-ФЗ, это не мелочь доступности.
  it("тумблеры сообщают своё состояние, а не только красятся", async () => {
    const think = document.querySelector("#think-toggle")!;
    const online = document.querySelector("#online-toggle")!;
    expect(think.getAttribute("aria-pressed"), "у «Размышлений» нет aria-pressed").toBe("false");
    expect(online.getAttribute("aria-pressed"), "у «Онлайн» нет aria-pressed").toBe("false");

    const { setToggleState } = await import("./ui");
    setToggleState(online as HTMLElement, true);
    expect(online.classList.contains("on")).toBe(true);
    expect(online.getAttribute("aria-pressed"), "класс сменился, а состояние — нет").toBe("true");

    setToggleState(online as HTMLElement, false);
    expect(online.getAttribute("aria-pressed")).toBe("false");
  });

  // Композер включался безусловно по завершении ответа. Если движок умирал во
  // время стрима, список моделей становился пуст, но поле ввода оживало: человек
  // печатал вопрос, жал Enter — и не происходило НИЧЕГО, ни ответа, ни ошибки.
  it("после ответа композер не включается, когда отвечать нечем", async () => {
    const dom = await import("./dom");
    dom.initDom();
    const { setStreaming } = await import("./ui");
    const { state } = await import("./state");

    state.selectedModel = "qwen3.5:4b";
    setStreaming(true);
    expect(dom.inputEl.disabled).toBe(true);

    // Модель пропала прямо во время ответа (движок перезапустился).
    state.selectedModel = "";
    setStreaming(false);
    expect(dom.inputEl.disabled, "поле ввода ожило без модели").toBe(true);
    expect(dom.sendBtn.disabled, "кнопка отправки ожила без модели").toBe(true);

    // А когда модель есть — всё как обычно.
    state.selectedModel = "qwen3.5:4b";
    setStreaming(true);
    setStreaming(false);
    expect(dom.inputEl.disabled).toBe(false);
    expect(dom.sendBtn.disabled).toBe(false);
  });
});
