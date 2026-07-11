// Ядро чата: сборка контекста (история + бюджет + RAG), отправка и стриминг
// ответа, OCR-запрос, авто-высота поля ввода.

import { Channel, invoke } from "@tauri-apps/api/core";
import type {
  ApiMsg,
  ChatEvent,
  Message,
  RetrievedChunk,
  Role,
  SourceRef,
  WebSource,
} from "./types";
import {
  history,
  state,
  thinkingByModel,
  toolsByModel,
  visionByModel,
  vramNotedModels,
} from "./state";
import {
  composerWrapEl,
  emptyStateEl,
  feedEl,
  imgOcrBtn,
  inputEl,
  messagesEl,
  stopBtn,
} from "./dom";
import { humanError } from "./util";
import {
  addAssistantTurn,
  addBubble,
  addError,
  addNotice,
  addTurnActions,
  renderMarkdownInto,
  renderSources,
  renderWebSources,
  scrollToBottom,
  setRetryHandler,
  setStreaming,
  stop,
} from "./ui";
import { anyVisionModel, clearPendingDoc, clearPendingImage } from "./attachments";
import { persist } from "./conversations";

// Системная подсказка — задаёт деловой тон ассистента и запрещает эмодзи.
const SYSTEM = {
  role: "system" as const,
  content:
    "Ты — деловой ассистент. Отвечай по-русски, профессионально и по существу. " +
    "Не используй эмодзи и смайлики.",
};

// Калибровка контекста. На токенайзере Qwen русский текст — это ~2 символа/токен
// (раньше в коде было заложено оптимистичное «3», из-за чего бюджеты завышались и
// со второго-третьего хода Ollama молча отбрасывала начало истории). Считаем по 2.
const NUM_CTX_DEFAULT = 8192; // потолок контекста (CLAUDE.md); при нехватке памяти снижается
const CHARS_PER_TOKEN = 2; // русский, консервативно
const ANSWER_RESERVE_CHARS = 3000; // ~1500 токенов оставляем под сам ответ модели
// Полный бюджет промпта в символах: система + история + контекст из базы + вопрос.
// Всё, что сверх — отбрасываем сами (управляемо), не отдавая на молчаливую обрезку Ollama.
const PROMPT_CHAR_BUDGET = NUM_CTX_DEFAULT * CHARS_PER_TOKEN - ANSWER_RESERVE_CHARS;

// Картинки (base64) огромны и быстро переполняют num_ctx. В контекст берём изображения
// только у последних N ходов пользователя — свежий вопрос про картинку работает,
// а старые кадры не копятся в каждом запросе.
const IMAGE_HISTORY_TURNS = 2;

// ── База документов (Фаза B5): RAG-поиск перед ответом ───────────────────────
// Сколько фрагментов искать и бюджет их суммарного объёма в контексте.
const RAG_TOP_K = 6;
const CONTEXT_CHAR_BUDGET = 5000; // ~2500 токенов (2 симв/ток) — с запасом под систему/вопрос/ответ
// Порог релевантности: vec0 отдаёт косинусную дистанцию (1 − cos), меньше = ближе.
// Фрагменты дальше порога считаем нерелевантными и в контекст не берём — иначе на
// любой вопрос (даже «привет») подмешивались бы top-6 и без нужды резалась история.
const RAG_MAX_DISTANCE = 0.6;

// Собирает массив сообщений для Ollama: система + история (+ контекст из базы),
// уложенная в бюджет символов PROMPT_CHAR_BUDGET. Всегда сохраняем систему, контекст
// из базы и ТЕКУЩИЙ ход; предыдущие ходы добавляем от новых к старым, пока помещаются
// (старые отбрасываем сами — управляемо, а не отдаём на молчаливую обрезку Ollama, из-за
// которой модель «забывала» начало диалога и приложенные файлы). Реплику с документом
// разворачиваем в текст «документ + вопрос»; объекты doc/sources в запрос не уходят.
function buildApiMessages(contextMsg: Message | null): ApiMsg[] {
  const n = history.length;

  // Картинки оставляем только у последних IMAGE_HISTORY_TURNS ходов с изображениями.
  const keepImages = new Set<number>();
  for (let i = n - 1, kept = 0; i >= 0 && kept < IMAGE_HISTORY_TURNS; i--) {
    if (history[i].images && history[i].images!.length) {
      keepImages.add(i);
      kept++;
    }
  }

  // Превращает сообщение истории в сообщение для Ollama (с разворотом документа).
  const materialize = (m: Message, i: number): ApiMsg => {
    let content: string;
    if (m.role === "user" && m.doc) {
      // Документ — справочный материал к ходу. Не приказываем «отвечай только по нему»
      // и не просим упоминать в каждом ответе: модель обращается к файлу по релевантности.
      content =
        `[Прикреплён документ «${m.doc.name}» — справочный материал, ` +
        `обращайся к нему, когда это относится к вопросу]\n\n` +
        `${m.doc.text}\n\n———\n\n${m.content}`;
    } else {
      content = m.content;
    }
    const item: ApiMsg = { role: m.role, content };
    if (m.role === "user" && m.images && m.images.length && keepImages.has(i)) {
      item.images = m.images;
    }
    return item;
  };

  const lastIdx = n - 1;
  const current = lastIdx >= 0 ? materialize(history[lastIdx], lastIdx) : null;
  const ctxItem: ApiMsg | null = contextMsg
    ? { role: contextMsg.role, content: contextMsg.content }
    : null;

  // Остаток бюджета под предыдущие ходы: вычитаем систему, контекст и текущий ход
  // (их сохраняем всегда). Картинки в бюджет символов не считаем — они в поле images.
  let budget = PROMPT_CHAR_BUDGET - SYSTEM.content.length;
  if (ctxItem) budget -= ctxItem.content.length;
  if (current) budget -= current.content.length;

  const older: ApiMsg[] = [];
  for (let i = lastIdx - 1; i >= 0; i--) {
    const item = materialize(history[i], i);
    if (item.content.length > budget) break; // дальше не помещается — старое отбрасываем
    budget -= item.content.length;
    older.unshift(item);
  }

  const out: ApiMsg[] = [SYSTEM, ...older];
  if (ctxItem) out.push(ctxItem); // фрагменты из базы — прямо перед текущим вопросом
  if (current) out.push(current);
  return out;
}

// Пакует найденные фрагменты в одно system-сообщение в рамках бюджета символов.
// Возвращает сообщение контекста и список источников (для показа под ответом).
function buildContext(retrieved: RetrievedChunk[]): {
  contextMsg: Message;
  sources: SourceRef[];
} {
  const parts: string[] = [];
  const sources: SourceRef[] = [];
  let used = 0;
  for (const r of retrieved) {
    const block = `[Документ «${r.filename}», фрагмент ${r.chunk_index + 1}]\n${r.text}`;
    if (parts.length && used + block.length > CONTEXT_CHAR_BUDGET) break; // бюджет
    parts.push(block);
    sources.push({ filename: r.filename, chunk_index: r.chunk_index });
    used += block.length;
  }
  const content =
    "Ниже — фрагменты из документов пользователя, которые могут относиться к его вопросу. " +
    "Если вопрос касается этих документов — отвечай, опираясь на фрагменты, и если нужного " +
    "ответа в них нет, честно скажи, что в документах это не найдено, и ничего не придумывай. " +
    "Если же вопрос не связан с этими фрагментами — просто ответь на него как обычно.\n\n" +
    parts.join("\n\n");
  return { contextMsg: { role: "system" as Role, content }, sources };
}

export async function send() {
  const text = inputEl.value.trim();
  if (!text || state.streaming || !state.selectedModel) return;

  inputEl.value = "";
  autoGrow();
  // Документ и/или изображение из композера привязываем к ЭТОМУ сообщению и сразу
  // убираем из поля ввода — они «уехали» вместе с вопросом.
  const doc = state.pendingDoc ?? undefined;
  const images = state.pendingImage ? [state.pendingImage] : undefined;
  clearPendingDoc();
  clearPendingImage();
  history.push({
    role: "user",
    content: text,
    ...(doc ? { doc } : {}),
    ...(images ? { images } : {}),
  });
  addBubble("user", text, doc, undefined, images);
  persist(); // вопрос (с файлом/картинкой) сохраняется сразу
  state.autoScroll = true; // при отправке снова следуем за ответом
  await generate();
}

// «Повторить»: убрать последний ответ ассистента из истории и ленты и сгенерировать
// заново на тот же вопрос (с текущими моделью/настройками — можно сменить и повторить).
export function regenerate() {
  if (state.streaming || !state.selectedModel) return;
  if (history[history.length - 1]?.role !== "assistant") return;
  history.pop();
  persist();
  const turns = messagesEl.querySelectorAll(".turn.ai");
  turns[turns.length - 1]?.remove();
  state.autoScroll = true;
  void generate();
}

// Генерация ответа на ПОСЛЕДНИЙ вопрос истории: RAG-поиск, бюджет контекста, оценка
// памяти, стрим (офлайн или агентный). Общий путь send() и regenerate().
async function generate() {
  // Запрос для поиска по базе — текст последнего вопроса пользователя (без документа).
  const queryText = [...history].reverse().find((m) => m.role === "user")?.content ?? "";

  const myGen = ++state.generation;
  setStreaming(true);

  // Ответ ассистента: индикатор «думаю», переливающееся рассуждение, текст.
  const ui = addAssistantTurn();
  let answer = "";
  let reasoning = "";
  let reasonExpanded = false;
  const startTs = Date.now();

  // Секундомер ожидания: загрузка модели и «Размышления» занимают десятки секунд —
  // без счётчика кажется, что приложение зависло. Первые секунды цифру не показываем
  // (быстрый ответ не должен мигать числами). Подпись-основа меняется статусами
  // агентного режима («Ищу в интернете…») — счётчик продолжает идти при них.
  let waitLabel = "Думаю над ответом";
  const waitLbl = ui.thinking.querySelector("span:last-child")!;
  const waitTimer = window.setInterval(() => {
    if (!ui.thinking.isConnected) {
      clearInterval(waitTimer); // индикатор убран (пошёл ответ/стоп) — счётчик не нужен
      return;
    }
    const sec = Math.round((Date.now() - startTs) / 1000);
    if (sec >= 4) waitLbl.textContent = `${waitLabel} · ${sec} с`;
  }, 1000);

  // Живая печать с форматированием: Markdown-рендер всего ответа на каждый токен
  // расточителен, поэтому перерисовываем не чаще раза в LIVE_RENDER_MS — глазу
  // этого достаточно, а слабое железо не захлёбывается. Финальный рендер по
  // завершении остаётся авторитетным.
  const LIVE_RENDER_MS = 180;
  let liveTimer: number | null = null;
  let lastLiveTs = 0;
  const paintLive = () => {
    lastLiveTs = Date.now();
    renderAnswer(answer);
    scrollToBottom();
  };
  const stopLivePaint = () => {
    if (liveTimer !== null) {
      clearTimeout(liveTimer);
      liveTimer = null;
    }
  };

  // Кнопка «Показать больше/меньше»: видна, только если рассуждение длиннее 3 строк.
  const syncToggle = () => {
    if (reasonExpanded) {
      ui.toggle.hidden = false;
      ui.toggle.textContent = "Показать меньше";
    } else {
      const overflows = ui.rbody.scrollHeight > ui.rbody.clientHeight + 1;
      ui.toggle.hidden = !overflows;
      ui.toggle.textContent = "Показать больше";
    }
  };
  ui.toggle.addEventListener("click", () => {
    reasonExpanded = !reasonExpanded;
    ui.rbody.classList.toggle("clamp", !reasonExpanded);
    syncToggle();
  });

  // Когда пошёл ответ — замораживаем «Рассуждение» в статичную кликабельную
  // подпись «Рассуждение · N сек» и СВОРАЧИВАЕМ текст рассуждения (ответ — главный).
  const freezeReason = (withTime: boolean) => {
    if (!reasoning || !ui.reasonWord.classList.contains("shimmer")) return;
    ui.reasonWord.classList.remove("shimmer");
    ui.reasonWord.classList.add("reason-done"); // кликабельно: раскрыть/скрыть
    const sec = Math.max(1, Math.round((Date.now() - startTs) / 1000));
    ui.reasonWord.textContent = withTime ? `Рассуждение · ${sec} сек` : "Рассуждение";
    ui.rbody.hidden = true;
    ui.toggle.hidden = true;
    reasonExpanded = false;
    ui.rbody.classList.add("clamp");
  };

  // Клик по застывшей подписи — показать/скрыть текст рассуждения.
  ui.reasonWord.addEventListener("click", () => {
    if (ui.reasonWord.classList.contains("shimmer")) return; // ещё думает — не трогаем
    const show = ui.rbody.hidden;
    ui.rbody.hidden = !show;
    if (show) syncToggle();
    else ui.toggle.hidden = true;
  });

  // Отрисовка ответа: Markdown, когда чанк рендера готов; при сбое — полный текст.
  const renderAnswer = (text: string) => renderMarkdownInto(ui.msg, text);

  const webSources: WebSource[] = []; // источники из веб-поиска (онлайн-режим)
  const onEvent = new Channel<ChatEvent>();
  onEvent.onmessage = (msg) => {
    if (myGen !== state.generation) return; // нажали «Стоп» — игнорируем хвост
    if (msg.type === "status") {
      // Агентный цикл сообщает стадию («Ищу в интернете…») — показываем в индикаторе;
      // секундомер выше подхватит новую подпись как основу.
      waitLabel = msg.content;
      waitLbl.textContent = msg.content;
    } else if (msg.type === "sources") {
      // Источники веб-поиска: копим без дублей по URL, рисуем под финальным ответом.
      for (const s of msg.items) {
        if (!webSources.some((x) => x.url === s.url)) webSources.push(s);
      }
    } else if (msg.type === "notice") {
      // Служебное уведомление от бэкенда (например, переполнение контекста).
      addNotice(msg.content);
    } else if (msg.type === "thinking") {
      reasoning += msg.content;
      ui.thinking.remove(); // индикатор теперь — переливающееся «Рассуждение»
      ui.reason.hidden = false;
      ui.rbody.textContent = reasoning;
      scrollToBottom();
    } else if (msg.type === "chunk") {
      answer += msg.content;
      if (answer) {
        ui.thinking.remove(); // пошёл ответ — убираем «Думаю…»
        freezeReason(true);
        // Живая печать с Markdown, но не чаще LIVE_RENDER_MS: свежий кусок либо
        // рисуем сразу (пауза прошла), либо ставим отложенную перерисовку.
        const since = Date.now() - lastLiveTs;
        if (since >= LIVE_RENDER_MS) {
          paintLive();
        } else if (liveTimer === null) {
          liveTimer = window.setTimeout(() => {
            liveTimer = null;
            if (myGen === state.generation) paintLive();
          }, LIVE_RENDER_MS - since);
        }
      }
      scrollToBottom();
    }
    // финал — по результату команды ниже (авторитетный полный ответ)
  };

  // RAG: при непустой базе ищем релевантные фрагменты ДО обращения к модели и
  // вставляем их как контекст. Поиск не должен ронять чат — при сбое идём обычным.
  let contextMsg: Message | null = null;
  let sources: SourceRef[] = [];

  // «Стоп» в любой момент запроса (поиск по базе, оценка памяти, генерация): убрать
  // индикатор «Думаю…», заморозить рассуждение и сохранить уже полученный частичный
  // ответ в историю — иначе «Думаю…» висит вечно, а ответ теряется (модель потом «не
  // помнит» свою реплику). Замыкание видит актуальные answer/reasoning/sources.
  state.activeStopCleanup = () => {
    stopLivePaint(); // отложенная перерисовка не должна ожить после «Стоп»
    ui.thinking.remove();
    freezeReason(true);
    if (reasoning) ui.rbody.textContent = reasoning;
    if (answer.trim()) {
      renderAnswer(answer);
      if (sources.length) renderSources(ui.turn, sources);
      if (webSources.length) renderWebSources(ui.turn, webSources);
      addTurnActions(ui.turn, answer); // частичный ответ тоже можно скопировать/повторить
      history.push({
        role: "assistant",
        content: answer,
        ...(sources.length ? { sources } : {}),
        ...(webSources.length ? { webSources } : {}),
      });
      persist();
    } else if (!reasoning.trim()) {
      ui.turn.remove(); // совсем пусто — убираем ход
    }
    scrollToBottom();
  };

  if (state.docsCount > 0) {
    try {
      // Поиск в базе ПРОЕКТА открытого чата (или общей, вне проектов — currentProjectId=null).
      const retrieved = await invoke<RetrievedChunk[]>("search_documents", {
        query: queryText,
        k: RAG_TOP_K,
        projectId: state.currentProjectId,
      });
      if (myGen !== state.generation) return; // остановили во время поиска
      // Берём только релевантные фрагменты (порог по косинусной дистанции). Если ни один
      // не прошёл — вопрос не про документы: идём обычным путём, историю не режем.
      const relevant = retrieved.filter((r) => r.distance <= RAG_MAX_DISTANCE);
      if (relevant.length) {
        const built = buildContext(relevant);
        contextMsg = built.contextMsg;
        sources = built.sources;
      }
    } catch (e) {
      if (myGen !== state.generation) return;
      addNotice(`Поиск по документам недоступен: ${humanError(e)}`);
    }
  }

  // Контекст для модели: система + история (урезанная при RAG) + контекст из базы.
  // У реплик с приложенным файлом текст документа вшивается в ход (как в Фазе A).
  const messages = buildApiMessages(contextMsg);

  // Инструкции проекта (если чат внутри проекта) — системным сообщением сразу после
  // базовой системы. Задают роль и правила для всех чатов этого проекта (как в Claude).
  if (state.currentProjectId) {
    const proj = state.projects.find((p) => p.id === state.currentProjectId);
    const instr = proj?.instructions.trim();
    if (instr) messages.splice(1, 0, { role: "system", content: instr });
  }

  // Авто-модель: ход с изображением выполняем на vision-модели, НЕ меняя выбор
  // пользователя в шапке (следующий текстовый вопрос вернётся к выбранной модели).
  // Смотрим на СОБРАННЫЕ сообщения — картинка может прийти и из недавней истории
  // (уточняющий вопрос про уже отправленное фото, без нового вложения).
  let baseModel = state.selectedModel;
  if (
    messages.some((m) => m.images && m.images.length > 0) &&
    !(visionByModel.get(baseModel) ?? false)
  ) {
    const vis = anyVisionModel();
    if (vis) baseModel = vis; // нет vision-модели — идём как есть (гейт был при прикреплении)
  }

  // Лестница смягчения (S2): ДО запуска оцениваем память по формуле. При нехватке —
  // снижаем контекст / подбираем модель полегче «вниз» / честно отказываем. Ручной
  // выбор не меняем: downscale действует только на этот запрос.
  let useModel = baseModel;
  let useCtx: number | undefined;
  let downscaleNote: string | null = null;
  try {
    const plan = await invoke<{
      action: string;
      model: string;
      num_ctx: number;
      reason: string | null;
      original_model: string;
      note: string | null;
    }>("plan_inference", { model: baseModel });
    if (myGen !== state.generation) return;
    if (plan.action === "refuse") {
      state.activeStopCleanup = null; // отказ до старта — дочистка не нужна
      ui.thinking.remove();
      ui.turn.remove();
      addNotice(
        `Не запускаю «${plan.original_model}»: ${plan.reason}. ` +
          `Выберите модель полегче или освободите память.`,
      );
      setStreaming(false);
      return;
    }
    useModel = plan.model;
    useCtx = plan.num_ctx;
    // Честное примечание (напр., «тесно в свободной видеопамяти — будет медленнее»):
    // показываем один раз на модель за сессию, чтобы не спамить каждый запрос.
    if (plan.note && !vramNotedModels.has(plan.model)) {
      vramNotedModels.add(plan.model);
      addNotice(plan.note);
    }
    if (plan.action === "downscale" && plan.reason) {
      const cap = (s: string) => s.charAt(0).toUpperCase() + s.slice(1);
      downscaleNote =
        plan.model !== plan.original_model
          ? `Выполнено на «${plan.model}»: ${plan.reason}.`
          : `${cap(plan.reason)}.`;
    }
  } catch {
    if (myGen !== state.generation) return;
    // оценка недоступна (нет Ollama и т.п.) — не блокируем, идём как есть
  }

  // Онлайн-режим: при включённом тумблере и модели с поддержкой инструментов идём
  // агентным путём (tool calling: веб-поиск). Иначе — обычный офлайн-стрим, как и
  // раньше. Модель без `tools` в онлайне → честное уведомление, чат работает обычным.
  const useTools = state.onlineMode && (toolsByModel.get(useModel) ?? false);
  if (state.onlineMode && !useTools) {
    addNotice(
      `Модель «${useModel}» не умеет вызывать инструменты — веб-поиск недоступен. ` +
        `Отвечаю офлайн. Для онлайн-инструментов выберите модель с поддержкой инструментов (например, qwen3.5:9b).`,
    );
  }
  // Онлайн: усиливаем поиск — подмешиваем системную подсказку с текущей датой, чтобы
  // модель искала, когда вопрос требует данных, и строила вывод на основе найденного.
  // Вставка ТОЛЬКО при агентном пути — офлайн-промпт остаётся без изменений.
  if (useTools) {
    const today = new Date().toLocaleDateString("ru-RU", {
      day: "numeric",
      month: "long",
      year: "numeric",
    });
    messages.splice(1, 0, {
      role: "system",
      content:
        `Сегодня ${today}. Онлайн-режим включён, доступен инструмент web_search (поиск в интернете). ` +
        `ВАЖНО: твои внутренние знания устарели и не содержат текущих данных реального мира. Поэтому для ` +
        `любого вопроса, ответ на который зависит от фактов — цены, курсы валют, новости, законы, даты, ` +
        `характеристики товаров и оборудования, статистика, события, «что/как/сколько/где/когда сейчас» — ` +
        `сначала обязательно вызови web_search по сути запроса. Один поиск возвращает до 20 источников — ` +
        `этого обычно достаточно для полного охвата; при необходимости охватить другой аспект сделай ещё ` +
        `один поиск (не больше двух всего). Затем обработай и сопоставь ВСЕ найденные источники вместе и ` +
        `дай развёрнутый ответ с анализом и обоснованным выводом (а не перечень ссылок), опираясь на источники ` +
        `и отмечая расхождения, если они есть. Не отвечай по памяти на фактические вопросы. Без поиска отвечай ` +
        `только на просьбы написать или переформулировать текст, посчитать, либо объяснить общее устойчивое понятие.`,
    });
  }

  try {
    // Возвращённое значение — ПОЛНЫЙ текст ответа (без гонок с доставкой канала).
    const full = await invoke<string>(useTools ? "agentic_chat" : "chat_stream", {
      model: useModel,
      messages,
      // think:true шлём ТОЛЬКО моделям, которые это поддерживают (иначе Ollama
      // вернёт ошибку «не умеет размышлять»).
      think: state.thinkEnabled && (thinkingByModel.get(useModel) ?? false),
      numCtx: useCtx, // рычаг смягчения (Rust: undefined → 8192)
      onEvent,
    });
    if (myGen === state.generation) {
      state.activeStopCleanup = null; // нормально завершились — дочистка «Стоп» не нужна
      stopLivePaint(); // финальный рендер ниже авторитетен — отложенный не нужен
      ui.thinking.remove();
      freezeReason(true);
      if (reasoning) ui.rbody.textContent = reasoning; // готов, раскрывается по клику
      answer = full; // авторитетный полный ответ
      if (answer.trim()) {
        renderAnswer(answer); // финальное форматирование один раз
        if (sources.length) renderSources(ui.turn, sources); // из каких документов взято
        if (webSources.length) renderWebSources(ui.turn, webSources); // из интернета
        addTurnActions(ui.turn, answer); // «Копировать» / «Повторить»
        history.push({
          role: "assistant",
          content: answer,
          ...(sources.length ? { sources } : {}),
          ...(webSources.length ? { webSources } : {}),
        });
        persist();
      } else if (!reasoning.trim()) {
        ui.turn.remove(); // совсем пусто — убираем
      }
      // Пост-фактум: спокойно сообщаем, что подобрали модель/контекст полегче.
      if (downscaleNote && (answer.trim() || reasoning.trim())) addNotice(downscaleNote);
      scrollToBottom();
      setStreaming(false);
    }
  } catch (err) {
    if (myGen !== state.generation) return;
    state.activeStopCleanup = null; // завершились с ошибкой — дочистка «Стоп» не нужна
    stopLivePaint();
    ui.thinking.remove();
    if (!answer && !reasoning) ui.turn.remove();
    addError(String(err));
    setStreaming(false);
  }
}

// OCR: «Извлечь текст» — тот же путь зрения, но с готовым OCR-промптом на русском.
const OCR_PROMPT =
  "Распознай и извлеки весь текст с изображения дословно, сохраняя структуру " +
  "(абзацы, списки, таблицы по возможности). Выведи только извлечённый текст.";

function ocrImage() {
  if (!state.pendingImage || state.streaming || !state.selectedModel) return;
  inputEl.value = OCR_PROMPT;
  autoGrow();
  send(); // vision-запрос с OCR-промптом + прикреплённая картинка
}

// Авто-высота поля ввода под текст.
export function autoGrow() {
  inputEl.style.height = "auto";
  inputEl.style.height = Math.min(inputEl.scrollHeight, 200) + "px";
}

// Обработчики композера и ленты: отправка, «Стоп», OCR, копирование кода,
// авто-прокрутка, чипы пустого состояния.
export function wireChat() {
  setRetryHandler(regenerate); // кнопка «Повторить» под ответами (ui.ts — без циклов)
  composerWrapEl.addEventListener("submit", (e) => {
    e.preventDefault();
    send();
  });
  stopBtn.addEventListener("click", stop);
  inputEl.addEventListener("input", autoGrow);
  inputEl.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      send();
    }
  });
  imgOcrBtn.addEventListener("click", ocrImage);

  // Кнопка «Копировать» в код-блоках (через делегирование).
  messagesEl.addEventListener("click", (e) => {
    const btn = (e.target as HTMLElement).closest(".copy") as HTMLButtonElement | null;
    if (!btn) return;
    const pre = btn.closest(".code")?.querySelector("pre");
    if (!pre) return;
    const restore = () => setTimeout(() => (btn.textContent = "Копировать"), 1500);
    navigator.clipboard
      .writeText(pre.textContent || "")
      .then(() => {
        btn.textContent = "✓ Скопировано";
        restore();
      })
      .catch(() => {
        btn.textContent = "Не удалось";
        restore();
      });
  });

  // Авто-следование за ответом включаем/выключаем по позиции прокрутки.
  feedEl.addEventListener("scroll", () => {
    state.autoScroll = feedEl.scrollHeight - feedEl.scrollTop - feedEl.clientHeight < 80;
  });

  // Чипы пустого состояния: подставляют текст в поле (без автоотправки).
  emptyStateEl.addEventListener("click", (e) => {
    const chip = (e.target as HTMLElement).closest(".chip") as HTMLButtonElement | null;
    if (!chip) return;
    inputEl.value = chip.dataset.prompt || chip.textContent?.trim() || "";
    autoGrow(); // существующий авто-ресайз поля
    inputEl.focus();
  });
}
