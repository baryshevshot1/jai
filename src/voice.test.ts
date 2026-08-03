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
      <div class="composer">
        <textarea id="chat-input" rows="1"></textarea>
        <span class="voice-state" id="voice-state" hidden><span class="voice-state__text"></span></span>
        <span class="sr-only" id="voice-announce" aria-live="polite"></span>
        <div class="composer-bar">
          <button type="button" class="tool voice-btn" id="voice-btn" hidden></button>
        </div>
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

// Подержать кнопку дольше порога защёлки (TAP_MS = 350 мс в voice.ts). Настоящая
// пауза, а не подмена часов: жест различается по РЕАЛЬНОМУ времени между нажатием
// и отпусканием, и подделывать здесь именно то, что проверяем, — значит не проверить.
const holdPast = () => new Promise((r) => setTimeout(r, 420));

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
    expect(voiceStateEl.textContent).toContain("Говорите…");

    await holdPast(); // именно удержание: быстрое отпускание теперь защёлкивает
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
    await holdPast();
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

    await holdPast();
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

describe("отмена диктовки и повторная инициализация", () => {
  beforeEach(() => {
    document.body.innerHTML = "";
  });

  // Раньше начатую диктовку нельзя было прекратить: единственным выходом было
  // договорить её до конца и стирать распознанное руками.
  it("Escape закрывает микрофон и НЕ вписывает сказанное", async () => {
    const { voiceBtn, voiceStateEl, inputEl } = await setup();
    inputEl.value = "уже набрано";
    press(voiceBtn);
    await flush();
    expect(voiceBtn.classList.contains("is-recording")).toBe(true);

    window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", cancelable: true }));
    await flush();

    expect(invokeMock.mock.calls.some(([cmd]) => cmd === "voice_cancel")).toBe(true);
    expect(invokeMock.mock.calls.some(([cmd]) => cmd === "voice_stop")).toBe(false);
    expect(inputEl.value, "отменённая диктовка не должна ничего дописывать").toBe("уже набрано");
    expect(voiceBtn.classList.contains("is-recording")).toBe(false);
    expect(voiceStateEl.hidden).toBe(true);
  });

  it("Escape вне записи ничего не отменяет", async () => {
    await setup();
    window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", cancelable: true }));
    await flush();
    expect(invokeMock.mock.calls.some(([cmd]) => cmd === "voice_cancel")).toBe(false);
  });

  // initVoice зовут второй раз после установки модели — чтобы кнопка появилась без
  // перезапуска приложения. Обработчики висят на ОКНЕ, и снять их некому.
  //
  // Считаем именно РЕГИСТРАЦИИ, а не вызовы команд: через команды это не видно.
  // Первая версия теста нажимала кнопку и проверяла, что voice_start ушёл один раз,
  // — и была зелёной даже с выключенной защитой, потому что повторный запуск и так
  // отсекается этапом диктовки. Тест, не воспроизводящий отказ, — не доказательство.
  it("повторная инициализация не навешивает обработчики окна заново", async () => {
    await setup();
    const voice = await import("./voice");

    const spy = vi.spyOn(window, "addEventListener");
    await voice.initVoice(); // как после установки модели из настроек
    await voice.initVoice();
    const again = spy.mock.calls.map(([type]) => type);
    spy.mockRestore();

    expect(again, `обработчики окна навешаны повторно: ${again.join(", ")}`).toEqual([]);
  });
});

describe("два способа диктовать одной кнопкой", () => {
  beforeEach(() => {
    document.body.innerHTML = "";
  });

  // Ради чего всё: длинную фразу неудобно наговаривать, держа палец на кнопке.
  it("короткое нажатие защёлкивает запись — отпускание её НЕ завершает", async () => {
    const { voiceBtn, voiceStateEl } = await setup();
    press(voiceBtn);
    await flush();
    releaseAnywhere(); // отпустили сразу же — это «тык»
    await flush();

    expect(invokeMock.mock.calls.some(([cmd]) => cmd === "voice_stop")).toBe(false);
    expect(voiceBtn.classList.contains("is-recording")).toBe(true);
    expect(voiceBtn.classList.contains("is-latched")).toBe(true);
    expect(voiceStateEl.textContent).toContain("нажмите, чтобы закончить");
  });

  it("второе нажатие завершает защёлкнутую запись", async () => {
    const { voiceBtn, inputEl } = await setup();
    press(voiceBtn);
    await flush();
    releaseAnywhere();
    await flush();

    press(voiceBtn); // второе нажатие
    await flush();
    releaseAnywhere();
    await flush();

    expect(invokeMock.mock.calls.filter(([cmd]) => cmd === "voice_stop")).toHaveLength(1);
    expect(inputEl.value).toBe("запиши это");
    expect(voiceBtn.classList.contains("is-latched")).toBe(false);
  });

  // Прежнее поведение обязано сохраниться: для короткой фразы удержание удобнее.
  it("удержание дольше порога работает как раньше — отпускание завершает", async () => {
    const { voiceBtn } = await setup();
    press(voiceBtn);
    await flush();
    await holdPast();
    releaseAnywhere();
    await flush();

    expect(invokeMock.mock.calls.some(([cmd]) => cmd === "voice_stop")).toBe(true);
    expect(voiceBtn.classList.contains("is-latched")).toBe(false);
  });

  it("Escape отменяет и защёлкнутую запись тоже", async () => {
    const { voiceBtn, inputEl } = await setup();
    inputEl.value = "было набрано";
    press(voiceBtn);
    await flush();
    releaseAnywhere();
    await flush();
    expect(voiceBtn.classList.contains("is-latched")).toBe(true);

    window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", cancelable: true }));
    await flush();

    expect(invokeMock.mock.calls.some(([cmd]) => cmd === "voice_cancel")).toBe(true);
    expect(inputEl.value).toBe("было набрано");
    expect(voiceBtn.classList.contains("is-latched")).toBe(false);
  });
});

describe("подпись во время записи", () => {
  it("всегда показывает время — микрофон открыт, и это должно быть видно", async () => {
    const { recordingLabel, clock } = await import("./voice");
    expect(clock(7)).toBe("0:07");
    expect(clock(75)).toBe("1:15");
    // Удержание: короткая подпись, палец и так на кнопке.
    expect(recordingLabel(7, false)).toBe("Говорите… 0:07");
    // Защёлка: подсказка «как закончить» показывается первые секунды…
    expect(recordingLabel(2, true)).toContain("нажмите, чтобы закончить");
    expect(recordingLabel(2, true)).toContain("0:02");
    // …а потом уходит: в строке композера тесно, и длинная подпись уводила кнопку
    // отправки вправо, ломая ряд. Счётчик и вид кнопки остаются.
    expect(recordingLabel(20, true)).toBe("Запись 0:20");
  });

  it("под конец предупреждает, что запись оборвётся сама", async () => {
    const { recordingLabel } = await import("./voice");
    // 120 с — потолок; за 10 секунд до него человек должен узнать заранее,
    // а не обнаружить обрыв на полуслове.
    expect(recordingLabel(110, true)).toBe("Осталось 10 с");
    expect(recordingLabel(110, false)).toBe("Осталось 10 с");
  });
});

// Регрессия вёрстки: длинная подпись во время записи распирала строку композера и
// уводила кнопку отправки вправо. Лечится тем, что текст живёт во ВЛОЖЕННОМ span —
// многоточие при нехватке места умеет только он, а не flex-контейнер снаружи.
describe("подпись не ломает строку композера", () => {
  it("текст пишется во вложенный span, а сам он переживает перерисовку", async () => {
    const { voiceBtn, voiceStateEl } = await setup();
    const inner = () => voiceStateEl.querySelector(".voice-state__text");
    expect(inner(), "в разметке нет .voice-state__text").toBeTruthy();

    press(voiceBtn);
    await flush();
    expect(inner(), "перерисовка снесла вложенный span").toBeTruthy();
    expect(inner()!.textContent).toContain("Говорите…");
    // На самом элементе собственного текстового узла быть не должно — иначе он
    // окажется вне обрезаемого блока и снова начнёт распирать ряд.
    const ownText = [...voiceStateEl.childNodes]
      .filter((n) => n.nodeType === 3)
      .map((n) => n.textContent?.trim())
      .join("");
    expect(ownText, "текст утёк мимо .voice-state__text").toBe("");
  });
});

// Открытый микрофон должен быть виден на уровне всей карточки композера: мелкую
// кнопку в углу боковым зрением не заметишь, рамку вокруг поля ввода — да.
// Для офлайнового продукта это не украшение, а часть обещания о приватности.
describe("видимость открытого микрофона", () => {
  beforeEach(() => {
    document.body.innerHTML = "";
  });

  it("во время записи подсвечена вся карточка композера, после — нет", async () => {
    const { voiceBtn } = await setup();
    const card = document.querySelector(".composer")!;
    expect(card.classList.contains("is-recording")).toBe(false);

    press(voiceBtn);
    await flush();
    expect(card.classList.contains("is-recording"), "карточка не подсветилась").toBe(true);

    await holdPast();
    releaseAnywhere();
    await flush();
    expect(card.classList.contains("is-recording"), "подсветка осталась после записи").toBe(false);
  });

  it("отмена по Escape тоже гасит подсветку", async () => {
    const { voiceBtn } = await setup();
    const card = document.querySelector(".composer")!;
    press(voiceBtn);
    await flush();
    releaseAnywhere(); // защёлка
    await flush();
    expect(card.classList.contains("is-recording")).toBe(true);

    window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", cancelable: true }));
    await flush();
    expect(card.classList.contains("is-recording")).toBe(false);
  });
});

// Счётчик времени обновляется четыре раза в секунду. Пока он жил в aria-live-области,
// экранный диктор всю диктовку перебивал сам себя — и заглушал в том числе
// предупреждение о скором обрыве записи. Теперь диктору сообщается только СМЕНА ЭТАПА.
describe("экранный диктор не тонет в счётчике", () => {
  beforeEach(() => {
    document.body.innerHTML = "";
  });

  it("бегущее время не попадает в живую область", async () => {
    const { voiceBtn, voiceStateEl } = await setup();
    const live = document.querySelector("#voice-announce")!;

    expect(
      voiceStateEl.getAttribute("aria-live"),
      "видимая подпись со счётчиком не должна быть живой областью",
    ).toBeNull();
    expect(live.getAttribute("aria-live")).toBe("polite");

    press(voiceBtn);
    await flush();
    expect(live.textContent).toBe("Идёт запись");
    // В видимой подписи время есть, в объявлении для диктора — нет.
    expect(voiceStateEl.textContent).toContain("0:0");
    expect(live.textContent).not.toContain("0:0");
  });

  it("повторная перерисовка не объявляет одно и то же заново", async () => {
    const { voiceBtn } = await setup();
    const live = document.querySelector("#voice-announce")!;
    press(voiceBtn);
    await flush();

    const before = live.textContent;
    let mutations = 0;
    new MutationObserver(() => mutations++).observe(live, {
      childList: true,
      characterData: true,
      subtree: true,
    });

    // Защёлкиваем: этап МЕНЯЕТСЯ — одно объявление уместно.
    releaseAnywhere();
    await flush();
    expect(live.textContent).not.toBe(before);

    // А дальше перерисовки с тем же этапом объявлять нечего.
    const afterLatch = live.textContent;
    mutations = 0;
    for (let i = 0; i < 5; i++) {
      window.dispatchEvent(new Event("resize")); // любой повод перерисоваться
      await flush();
    }
    expect(live.textContent).toBe(afterLatch);
    expect(mutations, "живая область меняется без смены этапа").toBe(0);
  });
});
