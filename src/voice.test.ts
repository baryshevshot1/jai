// Диктовка: проверяем ровно то, что можно проверить без микрофона — что интерфейс
// не теряет набранное, не даёт диктовать поверх идущего ответа и не оставляет
// запись включённой, когда кнопку отпустили мимо неё или ушли из окна.
// @vitest-environment jsdom
import { beforeEach, describe, expect, it, vi } from "vitest";

// Бэкенд подменяем целиком: в тестовой машине нет ни микрофона, ни модели
// распознавания, а проверять надо поведение окна — что и когда оно вызывает.
const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args?: unknown) => invokeMock(cmd, args),
  Channel: class {},
}));

// Разметка — минимальный слепок index.html: диктовке нужны кнопка, подпись, поле
// ввода и лента (в неё уходят уведомления об ошибках).
function mountApp() {
  document.body.innerHTML = `
    <div class="feed" id="feed"><div id="empty-state" hidden></div><div id="messages"></div></div>
    <form class="composer-wrap" id="chat-form">
      <textarea id="chat-input" rows="1"></textarea>
      <div class="composer-bar">
        <button type="button" class="tool voice-btn" id="voice-btn" hidden></button>
        <span class="voice-state" id="voice-state" hidden></span>
      </div>
    </form>`;
}

// Один оборот цикла событий: обработчики диктовки — цепочка промисов (старт →
// стоп → распознавание), и после клика им нужно дать доработать.
const flush = () => new Promise((r) => setTimeout(r, 0));

function press(btn: HTMLElement) {
  btn.dispatchEvent(new Event("pointerdown", { bubbles: true, cancelable: true }));
}
// Отпускаем НЕ на кнопке: курсор часто уезжает за её край, событие ловит окно.
function releaseAnywhere() {
  document.body.dispatchEvent(new Event("pointerup", { bubbles: true }));
}

type Ctx = {
  voiceBtn: HTMLButtonElement;
  voiceStateEl: HTMLElement;
  inputEl: HTMLTextAreaElement;
  state: { streaming: boolean };
};

// Поднимает окно с готовой к работе диктовкой (или без неё, если модель не стоит).
async function setup(available = true): Promise<Ctx> {
  mountApp();
  vi.resetModules();
  invokeMock.mockReset();
  invokeMock.mockImplementation(async (cmd: string) => {
    if (cmd === "voice_available") return available;
    if (cmd === "voice_start") return undefined;
    if (cmd === "voice_stop") return "запиши это";
    return undefined;
  });
  const dom = await import("./dom");
  dom.initDom();
  const voice = await import("./voice");
  const st = await import("./state");
  await voice.initVoice();
  return {
    voiceBtn: dom.voiceBtn,
    voiceStateEl: dom.voiceStateEl,
    inputEl: dom.inputEl,
    state: st.state,
  };
}

const started = () => invokeMock.mock.calls.filter(([cmd]) => cmd === "voice_start").length;

describe("joinDictated: надиктованное дописывается к набранному", () => {
  it("не затирает уже набранный текст и ставит один разделитель", async () => {
    const { joinDictated } = await import("./voice");
    expect(joinDictated("Черновик письма:", "добавь сроки")).toBe(
      "Черновик письма: добавь сроки",
    );
    expect(joinDictated("", "первая фраза")).toBe("первая фраза");
    // Разделитель уже стоит — второй не нужен (иначе после переноса строки
    // появлялся бы висячий пробел в начале строки).
    expect(joinDictated("список:\n", "первый пункт")).toBe("список:\nпервый пункт");
    expect(joinDictated("хвост ", "далее")).toBe("хвост далее");
  });

  it("пустое распознавание оставляет поле как было", async () => {
    const { joinDictated } = await import("./voice");
    expect(joinDictated("набрано руками", "   ")).toBe("набрано руками");
    expect(joinDictated("", "")).toBe("");
  });
});

describe("кнопка диктовки", () => {
  beforeEach(() => {
    document.body.innerHTML = "";
  });

  it("скрыта и молчит, пока модель распознавания не установлена", async () => {
    const { voiceBtn } = await setup(false);
    expect(voiceBtn.hidden).toBe(true);
    press(voiceBtn);
    releaseAnywhere();
    await flush();
    expect(started()).toBe(0); // нерабочая кнопка не должна ничего пытаться
  });

  it("во время записи видна активной, после отпускания — «Распознаю…»", async () => {
    const { voiceBtn, voiceStateEl } = await setup();
    let finish: (text: string) => void = () => {};
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "voice_stop") return new Promise<string>((r) => (finish = r));
      return undefined;
    });

    press(voiceBtn);
    await flush();
    expect(voiceBtn.classList.contains("is-recording")).toBe(true);
    expect(voiceStateEl.hidden).toBe(false);
    expect(voiceStateEl.textContent).toBe("Говорите…");

    releaseAnywhere();
    await flush();
    expect(voiceBtn.classList.contains("is-recording")).toBe(false);
    expect(voiceStateEl.textContent).toBe("Распознаю…");

    // Пока идёт распознавание, повторное нажатие ничего не начинает.
    press(voiceBtn);
    await flush();
    expect(started()).toBe(1); // всё та же одна запись, второй не появилось

    finish("готово");
    await flush();
    expect(voiceStateEl.hidden).toBe(true);
  });

  it("дописывает распознанное к набранному и ставит курсор в конец", async () => {
    const { voiceBtn, inputEl } = await setup();
    inputEl.value = "Черновик письма:";

    press(voiceBtn);
    await flush();
    releaseAnywhere();
    await flush();

    expect(inputEl.value).toBe("Черновик письма: запиши это");
    expect(inputEl.selectionStart).toBe(inputEl.value.length);
    expect(document.activeElement).toBe(inputEl);
  });

  it("не даёт диктовать, пока модель отвечает", async () => {
    const { voiceBtn, voiceStateEl, inputEl, state } = await setup();
    state.streaming = true;
    inputEl.disabled = true; // ровно это делает setStreaming в ui.ts
    await flush(); // наблюдателю за полем нужен оборот цикла

    expect(voiceBtn.disabled).toBe(true);
    press(voiceBtn);
    await flush();
    expect(started()).toBe(0);
    expect(voiceStateEl.hidden).toBe(true);
  });

  it("удержание пробела на кнопке работает как нажатие мышью", async () => {
    const { voiceBtn, inputEl } = await setup();
    const down = new KeyboardEvent("keydown", { key: " ", bubbles: true, cancelable: true });
    voiceBtn.dispatchEvent(down);
    await flush();
    expect(voiceBtn.classList.contains("is-recording")).toBe(true);
    // Автоповтор клавиши не должен начинать вторую запись.
    voiceBtn.dispatchEvent(
      new KeyboardEvent("keydown", { key: " ", bubbles: true, repeat: true }),
    );
    await flush();
    expect(started()).toBe(1);

    window.dispatchEvent(new KeyboardEvent("keyup", { key: " " }));
    await flush();
    expect(inputEl.value).toBe("запиши это");
  });

  it("уход из окна закрывает микрофон, а не оставляет запись висеть", async () => {
    const { voiceBtn, voiceStateEl } = await setup();
    press(voiceBtn);
    await flush();

    window.dispatchEvent(new Event("blur"));
    await flush();

    expect(invokeMock.mock.calls.some(([cmd]) => cmd === "voice_stop")).toBe(true);
    expect(voiceBtn.classList.contains("is-recording")).toBe(false);
    expect(voiceStateEl.hidden).toBe(true);
  });

  it("сбой микрофона показывает причину и возвращает кнопку в покой", async () => {
    const { voiceBtn, voiceStateEl } = await setup();
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "voice_start") throw { code: "unknown", message: "Микрофон не найден." };
      return undefined;
    });

    press(voiceBtn);
    await flush();

    expect(voiceBtn.classList.contains("is-recording")).toBe(false);
    expect(voiceStateEl.hidden).toBe(true);
    expect(document.querySelector(".err")?.textContent).toBe("Микрофон не найден.");
  });
});
