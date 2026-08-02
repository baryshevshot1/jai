// Рендер ответа модели — единственное место, где HTML из недоверенного источника
// попадает в innerHTML. Тесты держат обе стороны равновесия: исполняемого в разметке
// быть не должно, но формулы, подсветка кода и таблицы обязаны выжить.
//
// Проверяем РАЗОБРАННЫЙ DOM, а не подстроки: экранированный «<script>» остаётся в
// тексте страницы как видимый текст — это правильно и безопасно, и поиск подстроки
// ложно назвал бы такой рендер дырой.
// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import { renderMarkdown } from "./markdown";

function render(md: string): HTMLElement {
  const host = document.createElement("div");
  host.innerHTML = renderMarkdown(md);
  return host;
}

// Все атрибуты-обработчики (on*) во всём поддереве — их не должно быть ни одного.
function eventAttrs(root: HTMLElement): string[] {
  const found: string[] = [];
  for (const el of root.querySelectorAll("*")) {
    for (const a of el.attributes) if (a.name.toLowerCase().startsWith("on")) found.push(a.name);
  }
  return found;
}

describe("renderMarkdown: защита", () => {
  it("не создаёт элемент <script> из ответа модели", () => {
    const host = render('Вот ответ <script>alert(1)</script> и всё.');
    expect(host.querySelector("script")).toBeNull();
    // Текстом безобидное содержимое остаться может — важно, что оно не исполняется.
    expect(host.textContent).toContain("Вот ответ");
  });

  it("не создаёт ссылку со схемой javascript:", () => {
    const host = render("[нажми](javascript:alert(1))");
    const hrefs = [...host.querySelectorAll("a")].map((a) => a.getAttribute("href") ?? "");
    expect(hrefs.some((h) => h.toLowerCase().startsWith("javascript:"))).toBe(false);
  });

  it("не оставляет обработчиков событий в разметке", () => {
    const host = render('<img src=x onerror="alert(1)">');
    expect(eventAttrs(host)).toEqual([]);
  });

  it("не создаёт ссылку со схемой data:", () => {
    const host = render("[файл](data:text/html;base64,PHNjcmlwdD4=)");
    const hrefs = [...host.querySelectorAll("a")].map((a) => a.getAttribute("href") ?? "");
    expect(hrefs.some((h) => h.toLowerCase().startsWith("data:"))).toBe(false);
  });

  it("не оставляет обработчиков даже в разметке KaTeX и кода", () => {
    const host = render("$x^2$\n\n```js\nlet a = 1;\n```\n\n[ссылка](https://example.org)");
    expect(eventAttrs(host)).toEqual([]);
    expect(host.querySelector("script")).toBeNull();
  });
});

describe("renderMarkdown: ссылки открываются в системном браузере", () => {
  it("каждой ссылке проставляет target=_blank и rel", () => {
    const a = render("[сайт](https://example.org)").querySelector("a")!;
    expect(a.getAttribute("target")).toBe("_blank");
    expect(a.getAttribute("rel")).toContain("noopener");
  });

  it("голый URL (linkify) тоже получает target", () => {
    const a = render("Смотри https://example.org тут").querySelector("a")!;
    expect(a.getAttribute("target")).toBe("_blank");
    expect(a.getAttribute("href")).toContain("example.org");
  });
});

describe("renderMarkdown: содержимое не ломается", () => {
  it("сохраняет подсветку кода и кнопку копирования", () => {
    const host = render("```python\nprint('привет')\n```");
    expect(host.querySelector("pre.hljs")).not.toBeNull();
    expect(host.querySelector("button.copy")?.textContent).toBe("Копировать");
    expect(host.textContent).toContain("print");
  });

  it("содержимое код-блока показывает как текст, а не как разметку", () => {
    const host = render("```\n<script>alert(1)</script>\n```");
    expect(host.querySelector("script")).toBeNull();
    expect(host.querySelector("code")?.textContent).toContain("<script>alert(1)</script>");
  });

  it("рендерит формулу KaTeX и сохраняет её текстовое представление", () => {
    const host = render("Формула $E = mc^2$ в тексте.");
    expect(host.querySelector(".katex")).not.toBeNull();
    // MathML-ветка нужна для доступности и копирования формулы: если санитайзер
    // срежет semantics/annotation, формула станет «немой» для скринридера.
    expect(host.querySelector("annotation")).not.toBeNull();
  });

  it("сохраняет таблицы (сценарий «свести данные в таблицу»)", () => {
    const host = render("| а | б |\n|---|---|\n| 1 | 2 |");
    expect(host.querySelector("table")).not.toBeNull();
    expect(host.querySelectorAll("td").length).toBe(2);
  });

  it("не теряет обычное форматирование", () => {
    const host = render("**жирный** и *курсив*\n\n- пункт");
    expect(host.querySelector("strong")?.textContent).toBe("жирный");
    expect(host.querySelector("em")?.textContent).toBe("курсив");
    expect(host.querySelector("li")?.textContent).toContain("пункт");
  });
});
