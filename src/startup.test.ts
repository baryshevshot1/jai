// Проверка, что приложение вообще СТАРТУЕТ.
//
// Это тот класс отказов, который не ловит ни компилятор, ни остальные тесты: код
// типизирован, модули собираются, а при запуске падает первая же функция — и
// пользователь видит окно без половины интерфейса. Ровно так пропала карточка
// установки модели распознавания.
//
// Тест поднимает НАСТОЯЩУЮ разметку index.html, заполняет реестр элементов и
// вызывает те же wire*-функции, что и main.ts. Любое исключение здесь — это
// сломанный старт у пользователя.
// @vitest-environment jsdom
import { beforeEach, describe, expect, it, vi } from "vitest";
// Разметку берём через сборщик (?raw), а не через файловый API: типы Node в проекте
// не подключены намеренно — код работает в браузерном движке, и соблазн вызвать
// файловую систему из интерфейса должен отсутствовать даже в тестах.
import indexHtml from "../index.html?raw";

// Мосты в Rust в песочнице недоступны — подменяем их. Значения выбраны «пустыми»:
// нас интересует, что старт не падает, а не что показывает интерфейс.
vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(async (cmd: string) => {
    if (cmd === "voice_available") return false;
    if (cmd === "list_models") return [];
    if (cmd === "model_states") return { models: [], others: [] };
    if (cmd === "list_conversations") return [];
    if (cmd === "list_projects") return [];
    if (cmd === "list_documents") return [];
    if (cmd === "documents_empty") return true;
    if (cmd === "embedding_status") return false;
    if (cmd === "get_setting") return null;
    return null;
  }),
  Channel: class {
    onmessage: unknown = null;
  },
}));
vi.mock("@tauri-apps/plugin-dialog", () => ({ open: vi.fn(async () => null) }));
vi.mock("@tauri-apps/plugin-opener", () => ({ openUrl: vi.fn(async () => {}) }));
vi.mock("@tauri-apps/plugin-process", () => ({ relaunch: vi.fn(async () => {}) }));
vi.mock("@tauri-apps/plugin-updater", () => ({ check: vi.fn(async () => null) }));
vi.mock("@tauri-apps/api/webview", () => ({
  getCurrentWebview: () => ({ onDragDropEvent: vi.fn(async () => () => {}) }),
}));

function loadMarkup() {
  const body = indexHtml.slice(
    indexHtml.indexOf("<body>") + 6,
    indexHtml.indexOf("</body>"),
  );
  document.body.innerHTML = body;
}

describe("старт приложения", () => {
  beforeEach(() => {
    loadMarkup();
    vi.resetModules();
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
    const [chat, attachments, documents, models, conversations, projects, settings, wizard] =
      mods;

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
    const card = document.querySelector("#voice-model-install");
    expect(card, "карточка «Голосовой ввод» не построена").toBeTruthy();
  });

  it("инициализация голосового ввода не роняет старт без модели", async () => {
    const dom = await import("./dom");
    dom.initDom();
    const voice = await import("./voice");
    await expect(voice.initVoice()).resolves.not.toThrow();
    // Модели нет — кнопка микрофона должна остаться скрытой, а не пропасть совсем.
    expect(dom.voiceBtn.hidden).toBe(true);
  });
});
