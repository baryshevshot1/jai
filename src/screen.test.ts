// Переключение экранов: инвариант «виден ровно один» проверяем машиной, а не
// глазами. Раньше видимость складывалась из флагов hidden, расставленных вручную в
// четырёх модулях, и забытый флаг давал два экрана сразу или пустое окно.
// @vitest-environment jsdom
import { beforeEach, describe, expect, it } from "vitest";

// Разметка — минимальный слепок index.html: роутеру нужны только эти узлы.
function mountApp() {
  document.body.innerHTML = `
    <div class="feed" id="feed"></div>
    <form class="composer-wrap" id="chat-form"></form>
    <section class="settings-view" id="settings-view" hidden></section>
    <section class="settings-view" id="wizard-view" hidden></section>
    <section class="settings-view project-view" id="project-view" hidden></section>
    <div class="project-list" id="project-list"></div>
    <div class="settings-status" id="models-status" hidden></div>
    <button type="button" class="iconbtn" id="settings-btn"></button>`;
}

const SCREENS = ["chat", "settings", "project", "wizard"] as const;
type Screen = (typeof SCREENS)[number];

// Какой узел отвечает за каждый экран (чат — это лента вместе с полем ввода).
const NODES: Record<Screen, string[]> = {
  chat: ["#feed", "#chat-form"],
  settings: ["#settings-view"],
  project: ["#project-view"],
  wizard: ["#wizard-view"],
};

function visible(sel: string): boolean {
  return !(document.querySelector(sel) as HTMLElement).hidden;
}

// Экраны, показанные прямо сейчас.
function shown(): Screen[] {
  return SCREENS.filter((s) => NODES[s].every(visible));
}

describe("showScreen", () => {
  let showScreen: (s: Screen) => void;
  let activeScreen: () => Screen;
  let state: { viewingProjectId: string | null };

  beforeEach(async () => {
    mountApp();
    // Реестр DOM заполняется явным initDom() — таков контракт dom.ts. Кэш модулей
    // сбрасываем, иначе второй тест держал бы узлы, оставшиеся от первого.
    const vitest = await import("vitest");
    vitest.vi.resetModules();
    const dom = await import("./dom");
    dom.initDom(); // узлов вне слепка нет — они и не нужны переключателю экранов
    const ui = await import("./ui");
    const st = await import("./state");
    showScreen = ui.showScreen as (s: Screen) => void;
    activeScreen = ui.activeScreen as () => Screen;
    state = st.state;
  });

  it("на каждом экране виден ровно один — из любого в любой", () => {
    // Полный перебор переходов, включая переход экрана в самого себя.
    for (const from of SCREENS) {
      for (const to of SCREENS) {
        showScreen(from);
        showScreen(to);
        expect(shown(), `переход ${from} → ${to}`).toEqual([to]);
        expect(activeScreen()).toBe(to);
      }
    }
  });

  it("поле ввода доступно только в чате", () => {
    showScreen("settings");
    expect(visible("#chat-form")).toBe(false);
    showScreen("chat");
    expect(visible("#chat-form")).toBe(true);
  });

  it("кнопка настроек подсвечена ровно на экране настроек", () => {
    const btn = document.querySelector("#settings-btn") as HTMLElement;
    showScreen("settings");
    expect(btn.classList.contains("active")).toBe(true);
    showScreen("project");
    expect(btn.classList.contains("active")).toBe(false);
  });

  it("уход с экрана проекта снимает выбор и подсветку строки", () => {
    document.querySelector("#project-list")!.innerHTML =
      '<div class="project active"></div><div class="project"></div>';
    showScreen("project");
    state.viewingProjectId = "p1";

    showScreen("chat");
    expect(state.viewingProjectId).toBeNull();
    expect(document.querySelectorAll(".project.active").length).toBe(0);
  });

  it("возврат на экран проекта не сбрасывает выбранный проект", () => {
    showScreen("project");
    state.viewingProjectId = "p1";
    showScreen("project"); // повторное открытие того же экрана
    expect(state.viewingProjectId).toBe("p1");
  });

  it("итог операции из настроек не переезжает на другой экран", () => {
    const status = document.querySelector("#models-status") as HTMLElement;
    showScreen("settings");
    status.hidden = false; // как будто показали результат установки модели
    showScreen("chat");
    expect(status.hidden).toBe(true);
  });
});
