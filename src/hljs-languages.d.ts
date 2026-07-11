// Типы для per-language модулей highlight.js (lib/languages/*): каждый экспортирует
// функцию-определение языка. Нужен для строгого tsc при импорте отдельных языков.
declare module "highlight.js/lib/languages/*" {
  import type { LanguageFn } from "highlight.js";
  const language: LanguageFn;
  export default language;
}
