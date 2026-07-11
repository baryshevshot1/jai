// Точка сборки приложения: подключение шрифтов, инициализация DOM-реестра и
// контекстов, привязка обработчиков модулей и стартовая последовательность.
// Логика живёт в модулях: chat / conversations / projects / documents / models /
// attachments / settings / online / pull, база — types / state / dom / util / ui.
//
// Тяжёлый Markdown-рендер (katex, highlight.js) сюда НЕ импортируется — он уезжает
// в отдельный ленивый чанк (см. ui.ts → import("./markdown")).

// Шрифты — локально (бандлятся Vite), без сети: Inter (UI), Fraunces (бренд), JetBrains Mono (код).
import "@fontsource/inter/400.css";
import "@fontsource/inter/500.css";
import "@fontsource/inter/600.css";
import "@fontsource/inter/700.css";
import "@fontsource/fraunces/500.css";
import "@fontsource/fraunces/600.css";
import "@fontsource/jetbrains-mono/400.css";
import "@fontsource/jetbrains-mono/500.css";

import { initDom, installEmbedBtn } from "./dom";
import { setComposerEnabled } from "./ui";
import { wireChat } from "./chat";
import { wireAttachments } from "./attachments";
import {
  initDocContexts,
  installEmbeddingModel,
  refreshCurrentDocsCount,
  refreshDocuments,
  sidebarDocCtx,
  wireDocuments,
} from "./documents";
import { checkOllama, ensureEngine, loadHardware, loadModels, wireModels } from "./models";
import { initConversations, wireConversations } from "./conversations";
import { refreshProjects, wireProjects } from "./projects";
import { initSidebar, initTheme, initThinking, wireSettings } from "./settings";
import { initOnline } from "./online";
import { maybeOfferWizard, wireWizard } from "./wizard";

window.addEventListener("DOMContentLoaded", async () => {
  initDom(); // реестр элементов — строго первым
  initDocContexts(); // контексты документов (сайдбар/проект) — из готовых рефов

  // Обработчики модулей (каждый навешивает свои).
  wireChat();
  wireAttachments();
  wireDocuments();
  wireModels();
  wireConversations();
  wireProjects();
  wireSettings();
  wireWizard();
  // Композиция поверх слоёв: documents не импортирует models, поэтому обновление
  // списка моделей после успешной установки bge-m3 делает точка сборки.
  installEmbedBtn.addEventListener("click", async () => {
    if ((await installEmbeddingModel()) === "done") await loadModels();
  });

  initTheme(); // применяем сохранённую/системную тему как можно раньше
  initThinking(); // восстанавливаем тумблер «Размышления» из настроек (+миграция)
  initOnline(); // восстанавливаем онлайн-режим и настройки веб-поиска (по умолчанию офлайн)
  initSidebar(); // восстанавливаем ширину и состояние левой панели

  setComposerEnabled(false); // включим, когда загрузится список моделей
  await refreshProjects(); // список проектов в боковой панели
  await initConversations(); // сначала восстановим диалоги в ленту
  loadHardware(); // неблокирующе: светофор железа (локально, без движка)
  // Сначала обеспечиваем движок (поднимаем свой или переиспользуем системный), затем
  // уже опираемся на него. Если не готов — статус выставлен, движок-зависимые шаги
  // пропускаем (пользователь может повторить кнопкой «Проверка»).
  const engineReady = await ensureEngine();
  if (engineReady) {
    checkOllama(); // покажет версию Ollama в шапке
    loadModels();
    refreshDocuments(sidebarDocCtx); // общая база (вне проектов) + статус модели эмбеддингов
    refreshCurrentDocsCount(); // есть ли документы у открытого чата (для RAG)
  }
  // Первый запуск: обязательных моделей нет → мастер установки (оценит машину,
  // предложит посильный набор, поставит с флешки или из интернета). Сам молчит,
  // если движок недоступен — тогда мастер открывается кнопкой из настроек.
  maybeOfferWizard();
});
