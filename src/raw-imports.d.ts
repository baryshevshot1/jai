// Импорт файла строкой (суффикс ?raw у Vite). Нужен тестам, которые поднимают
// настоящую разметку index.html, чтобы проверить старт приложения.
declare module "*?raw" {
  const content: string;
  export default content;
}
