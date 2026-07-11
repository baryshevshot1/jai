// Рендер ответа ассистента: Markdown + формулы (KaTeX) + подсветка кода.
// Всё локально (бандлится Vite) — без сети, офлайн-правило соблюдено.
//
// Модуль грузится ЛЕНИВО (динамический import в ui.ts): markdown-it, KaTeX,
// highlight.js и их CSS уезжают в отдельный чанк и не тормозят старт приложения.
import "katex/dist/katex.min.css";
import "highlight.js/styles/atom-one-dark.css";
import MarkdownIt from "markdown-it";
import texmath from "markdown-it-texmath";
import katex from "katex";
// highlight.js: ядро + ЯВНЫЙ набор языков вместо lib/common (~40 языков — лишний
// объём). Набор — под делового ассистента с навыком кода (плюс 1С для российского
// бизнеса). Неизвестный язык не проблема: блок выводится экранированным текстом.
import hljs from "highlight.js/lib/core";
import python from "highlight.js/lib/languages/python";
import javascript from "highlight.js/lib/languages/javascript";
import typescript from "highlight.js/lib/languages/typescript";
import bash from "highlight.js/lib/languages/bash";
import sql from "highlight.js/lib/languages/sql";
import json from "highlight.js/lib/languages/json";
import yaml from "highlight.js/lib/languages/yaml";
import xml from "highlight.js/lib/languages/xml";
import css from "highlight.js/lib/languages/css";
import java from "highlight.js/lib/languages/java";
import c from "highlight.js/lib/languages/c";
import cpp from "highlight.js/lib/languages/cpp";
import csharp from "highlight.js/lib/languages/csharp";
import rust from "highlight.js/lib/languages/rust";
import go from "highlight.js/lib/languages/go";
import php from "highlight.js/lib/languages/php";
import onec from "highlight.js/lib/languages/1c";

// Алиасы (js/ts/py/html и т. п.) приходят из определений самих языков.
hljs.registerLanguage("python", python);
hljs.registerLanguage("javascript", javascript);
hljs.registerLanguage("typescript", typescript);
hljs.registerLanguage("bash", bash);
hljs.registerLanguage("sql", sql);
hljs.registerLanguage("json", json);
hljs.registerLanguage("yaml", yaml);
hljs.registerLanguage("xml", xml);
hljs.registerLanguage("css", css);
hljs.registerLanguage("java", java);
hljs.registerLanguage("c", c);
hljs.registerLanguage("cpp", cpp);
hljs.registerLanguage("csharp", csharp);
hljs.registerLanguage("rust", rust);
hljs.registerLanguage("go", go);
hljs.registerLanguage("php", php);
hljs.registerLanguage("1c", onec);

// Экранирование для код-блоков без подсветки (не ссылаемся на md, чтобы
// не создавать циклическую зависимость в его же инициализаторе).
function escapeHtml(s: string): string {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;");
}

const md = new MarkdownIt({
  // html: false ⇒ сырой HTML из ответа экранируется (защита от XSS),
  // потом результат безопасно вставляем через innerHTML.
  html: false,
  linkify: true,
  breaks: true,
});

// Код-блок: обёртка .code с шапкой (язык + «Копировать») и подсветкой.
md.renderer.rules.fence = (tokens, idx) => {
  const token = tokens[idx];
  const lang = (token.info || "").trim().split(/\s+/)[0] || "text";
  let body: string;
  if (lang !== "text" && hljs.getLanguage(lang)) {
    try {
      body = hljs.highlight(token.content, { language: lang, ignoreIllegals: true }).value;
    } catch {
      body = escapeHtml(token.content);
    }
  } else {
    body = escapeHtml(token.content);
  }
  return (
    '<div class="code"><div class="code-head"><span>' +
    escapeHtml(lang) +
    '</span><button class="copy" type="button">Копировать</button></div>' +
    '<pre class="hljs"><code>' +
    body +
    "</code></pre></div>"
  );
};

// Формулы $…$ и $$…$$ через KaTeX. throwOnError: false — кривую формулу
// показываем как есть, без падения рендера.
md.use(texmath, {
  engine: katex,
  delimiters: "dollars",
  katexOptions: { throwOnError: false },
});

export function renderMarkdown(text: string): string {
  return md.render(text);
}
