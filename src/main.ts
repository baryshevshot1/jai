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

import { installErrorReporting, report, runBoot, showBootFailure } from "./boot";
import { initDom, installEmbedBtn } from "./dom";
import { setBootStep, setComposerEnabled } from "./ui";
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
import {
  checkOllama,
  ensureEngine,
  initModelChoice,
  loadHardware,
  loadModels,
  wireModels,
} from "./models";
import { initConversations, wireConversations } from "./conversations";
import { refreshProjects, wireProjects } from "./projects";
import { initGentle, initSidebar, initTheme, initThinking, wireSettings } from "./settings";
import { initOnline } from "./online";
import { initVoice } from "./voice";
import { maybeOfferWizard, wireWizard } from "./wizard";

window.addEventListener("DOMContentLoaded", async () => {
  installErrorReporting(); // до всего: ошибки самого старта тоже должны попасть в журнал
  report("info", "старт", "инициализация окна");

  // Реестр элементов — строго первым и БЕЗ изоляции: если разметка и реестр
  // разошлись, дальше идти некуда, и честнее упасть с внятной плашкой, чем
  // возводить интерфейс поверх пустоты.
  try {
    initDom();
  } catch (e) {
    await showBootFailure([
      { name: "разметка окна", error: e instanceof Error ? e.message : String(e) },
    ]);
    report("error", "старт", `реестр элементов не собрался: ${e}`);
    return;
  }

  // Дальше — каждый модуль в своей изоляции. Раньше это была одна цепочка, и
  // исключение в любом звене отменяло все следующие: интерфейс приезжал наполовину,
  // а место отказа приходилось угадывать.
  const failures = await runBoot([
    { name: "контексты документов", run: initDocContexts },
    { name: "чат", run: wireChat },
    { name: "вложения", run: wireAttachments },
    { name: "документы", run: wireDocuments },
    { name: "модели", run: wireModels },
    { name: "история диалогов", run: wireConversations },
    { name: "проекты", run: wireProjects },
    { name: "настройки", run: wireSettings },
    { name: "мастер установки", run: wireWizard },
    {
      // Композиция поверх слоёв: documents не импортирует models, поэтому обновление
      // списка моделей после успешной установки bge-m3 делает точка сборки.
      name: "связка «модель документов»",
      run: () =>
        installEmbedBtn.addEventListener("click", async () => {
          if ((await installEmbeddingModel()) === "done") await loadModels();
        }),
    },
    { name: "тема оформления", run: initTheme },
    { name: "тумблер «Размышления»", run: initThinking },
    // Щадящий режим — ДО светофора железа (авто-решение учитывает выбор),
    // выбор модели — ДО подбора модели ниже. Порядок значим, изоляция его не меняет.
    { name: "щадящий режим", run: initGentle },
    { name: "выбор модели", run: initModelChoice },
    { name: "онлайн-режим", run: initOnline },
    { name: "голосовой ввод", run: initVoice },
    { name: "боковая панель", run: initSidebar },
  ]);
  await showBootFailure(failures);

  setComposerEnabled(false); // включим, когда загрузится список моделей
  await refreshProjects(); // список проектов в боковой панели
  await initConversations(); // сначала восстановим диалоги в ленту
  // Светофор железа — параллельно с движком (не блокирует его запуск), но его результат
  // нужен ДО подбора модели: уровень решает, брать старшую модель или лёгкую.
  const hwReady = loadHardware();
  // Сначала обеспечиваем движок (поднимаем свой или переиспользуем системный), затем
  // уже опираемся на него. Если не готов — статус выставлен, движок-зависимые шаги
  // пропускаем (пользователь может повторить кнопкой обновления в шапке).
  // Шаги запуска на приветственном экране: холодный старт длится секунды, и
  // мелкой подписи в шапке мало — без видимого хода дела это выглядит как «зависло».
  setBootStep("engine");
  const engineReady = await ensureEngine();
  if (engineReady) {
    setBootStep("models");
    checkOllama(); // покажет версию Ollama в шапке
    await hwReady; // уровень железа известен → авто-выбор модели сразу верный, без «скачка»
    // Список моделей не задерживает остальной старт, но именно он закрывает
    // последний шаг: после него приложение действительно готово отвечать.
    void loadModels().then(
      () => {
        setBootStep("ready");
        window.setTimeout(() => setBootStep("off"), 1500); // подержать «готов» и убрать
      },
      () => setBootStep("models", true),
    );
    refreshDocuments(sidebarDocCtx); // общая база (вне проектов) + статус модели эмбеддингов
    refreshCurrentDocsCount(); // есть ли документы у открытого чата (для RAG)
  } else {
    setBootStep("engine", true); // шаги остаются на экране с подсказкой, куда идти
  }
  // Первый запуск: обязательных моделей нет → мастер установки (оценит машину,
  // предложит посильный набор, поставит с флешки или из интернета). Сам молчит,
  // если движок недоступен — тогда мастер открывается кнопкой из настроек.
  maybeOfferWizard();
});
