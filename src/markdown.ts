// Рендер ответа ассистента: Markdown + формулы (KaTeX) + подсветка кода.
// Всё локально (бандлится Vite) — без сети, офлайн-правило соблюдено.
import MarkdownIt from "markdown-it";
import texmath from "markdown-it-texmath";
import katex from "katex";
import hljs from "highlight.js/lib/common";

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
