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
  highlight(str: string, lang: string): string {
    if (lang && hljs.getLanguage(lang)) {
      try {
        return (
          '<pre class="hljs"><code>' +
          hljs.highlight(str, { language: lang, ignoreIllegals: true }).value +
          "</code></pre>"
        );
      } catch {
        /* падать на подсветке нельзя — ниже отдадим экранированный текст */
      }
    }
    return '<pre class="hljs"><code>' + escapeHtml(str) + "</code></pre>";
  },
});

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
