// Диалоги: сохранение/загрузка/переключение/удаление, список в боковой панели,
// список чатов проекта и чип проекта в шапке (обе последние вещи живут здесь,
// чтобы projects.ts зависел от conversations.ts, а не наоборот — без циклов).

import { invoke } from "@tauri-apps/api/core";
import type { Conversation, ConversationMeta, Message } from "./types";
import { history, shownSourceFiles, state } from "./state";
import {
  chatProjectChip,
  chatProjectChipName,
  clearChatsBtn,
  convListEl,
  convSearchEl,
  inputEl,
  messagesEl,
  newChatBtn,
  projectChatListEl,
  projectView,
} from "./dom";
import {
  addBubble,
  addError,
  confirmModal,
  refreshEmptyState,
  showChatView,
  stop,
} from "./ui";
import { refreshCurrentDocsCount } from "./documents";
import { humanError, makePressable } from "./util";

// Кэш списка диалогов и текущий фильтр поиска (для отрисовки боковой панели).
let convMetas: ConversationMeta[] = [];
let convFilter = "";

// Заголовок диалога — из первого вопроса пользователя (обрезанный).
function titleFromHistory(): string {
  const first = history.find((m) => m.role === "user");
  if (!first) return "Новый диалог";
  const t = first.content.trim().replace(/\s+/g, " ");
  // Режем по кодовым точкам: slice по UTF-16-единицам может разрубить эмодзи на
  // границе, а одиночный суррогат не переживает сериализацию IPC — сохранение падает.
  const cps = [...t];
  return cps.length > 40 ? cps.slice(0, 40).join("") + "…" : t;
}

// Ид удалённых диалогов: поздний автосейв (черновик, дочистка «Стопа») не должен
// воскресить стёртый пользователем файл — записи по этим id запрещены навсегда.
const deletedIds = new Set<string>();

// Очередь записей на диск: параллельные save_conversation не упорядочены на
// бэкенде, и черновик, стартовавший за миг до финала стрима, мог бы записаться
// ПОСЛЕ финального сохранения — файл остался бы с усечённым ответом.
let saveChain: Promise<unknown> = Promise.resolve();

// Об отказе сохранения предупреждаем в ленте один раз: автосейв повторяется
// каждые несколько секунд — спамить ошибкой нельзя, но и молчать нельзя.
let saveErrorShown = false;

// Сохраняет текущий диалог на диск и обновляет список. Пустой не сохраняем.
// draft — незавершённый ответ ассистента (автосейв во время генерации): пишется
// в файл ПОВЕРХ истории, но в саму history не попадает — финальное сохранение
// по завершении перезапишет файл начисто. Список диалогов черновик не трогает.
export async function persist(draft?: Message) {
  if (!history.length || !state.currentId || deletedIds.has(state.currentId)) return;
  const conv: Conversation = {
    id: state.currentId,
    title: titleFromHistory(),
    updated_at: Date.now(),
    ...(state.currentProjectId ? { project_id: state.currentProjectId } : {}),
    // Снимок массива: запись уходит через очередь, к её моменту history мог измениться.
    messages: draft ? [...history, draft] : [...history],
  };
  const write = saveChain
    .catch(() => {}) // сбой прежней записи не должен закупорить очередь
    .then(() => {
      // Повторная проверка в момент фактической записи: диалог могли удалить,
      // пока очередь доходила до этого сохранения.
      if (deletedIds.has(conv.id)) return;
      return invoke("save_conversation", { conversation: conv });
    });
  saveChain = write;
  try {
    await write;
    saveErrorShown = false; // запись снова проходит: о следующем сбое стоит сказать
    if (!draft) await refreshConversationList();
  } catch (e) {
    console.error("save_conversation:", e);
    // Отказ записи не должен быть немым — диалог молча перестал бы сохраняться.
    if (!saveErrorShown) {
      saveErrorShown = true;
      addError(`Не удалось сохранить диалог: ${humanError(e)}`);
    }
  }
}

// Перерисовывает ленту по текущему массиву history.
function renderHistory() {
  messagesEl.innerHTML = "";
  shownSourceFiles.clear(); // заново считаем «первое упоминание» источников в этом диалоге
  state.autoScroll = true; // открыли диалог — показываем низ (последние сообщения)
  for (const m of history)
    addBubble(m.role, m.content, m.doc, m.sources, m.images, m.webSources);
  refreshEmptyState(); // пустой диалог → приветствие; иначе скрыто
}

// Перечитывает список диалогов из файлов и перерисовывает боковую панель.
export async function refreshConversationList() {
  try {
    convMetas = await invoke<ConversationMeta[]>("list_conversations");
  } catch {
    return;
  }
  renderConvList();
  if (!projectView.hidden && state.viewingProjectId) renderProjectChats(state.viewingProjectId);
}

// Шкала времени списка диалогов: полоса «от какого момента» и её подпись.
type DateBand = { from: number; label: string };

// Границы шкалы — локальные полуночи N суток назад: диалог вчерашнего вечера
// попадает во «Вчера» и утром, и в обед, а не по счётчику «24 часа назад».
// Сутки отматываем через setDate, а не вычитанием миллисекунд: при переходе на
// летнее время в сутках не 24 часа и граница уехала бы на час.
function dateScale(): DateBand[] {
  const midnightDaysAgo = (days: number) => {
    const d = new Date();
    d.setHours(0, 0, 0, 0);
    d.setDate(d.getDate() - days);
    return d.getTime();
  };
  return [
    { from: midnightDaysAgo(0), label: "Сегодня" },
    { from: midnightDaysAgo(1), label: "Вчера" },
    { from: midnightDaysAgo(7), label: "На этой неделе" },
    { from: midnightDaysAgo(30), label: "В этом месяце" },
  ];
}

// Группа по дате последнего изменения. Шкалу передаём снаружи: она считается
// один раз на отрисовку — иначе список, начатый за миг до полуночи, получил бы
// две разные группы «Сегодня».
function dateGroup(ts: number, scale: DateBand[]): string {
  return scale.find((band) => ts >= band.from)?.label ?? "Ранее";
}

const ICON_CHAT =
  '<path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z"/>';

// Рисует список БЫСТРЫХ чатов (вне проектов) с учётом поиска и групп по датам.
// Чаты проектов показываются на экране проекта, не в общем списке.
function renderConvList() {
  convListEl.innerHTML = "";
  const f = convFilter.trim().toLowerCase();
  const items = convMetas
    .filter((m) => !m.project_id) // только вне проектов
    .filter((m) => !f || (m.title || "").toLowerCase().includes(f))
    // Группы идут сверху вниз от свежих к старым, поэтому порядок обязан быть
    // строго убывающим: иначе одна и та же подпись («Вчера») встретилась бы дважды.
    .sort((a, b) => b.updated_at - a.updated_at);

  // Пустой список не должен выглядеть поломкой: у первого запуска — подсказка,
  // у поиска без результатов — честное «ничего не найдено».
  if (items.length === 0) {
    const empty = document.createElement("div");
    empty.className = "conv-empty";
    empty.textContent = f
      ? "По запросу ничего не найдено."
      : "Чатов пока нет. Нажмите «Новый чат» и задайте первый вопрос.";
    convListEl.appendChild(empty);
    return;
  }

  // Подпись группы добавляется вместе с ПЕРВЫМ её чатом, поэтому после поиска
  // не остаётся заголовков без содержимого: пустая группа просто не встречается.
  const scale = dateScale();
  let lastGroup = "";
  for (const m of items) {
    const group = dateGroup(m.updated_at, scale);
    if (group !== lastGroup) {
      const lbl = document.createElement("div");
      lbl.className = "conv-label";
      lbl.textContent = group;
      convListEl.appendChild(lbl);
      lastGroup = group;
    }

    const item = document.createElement("div");
    item.className = "conv" + (m.id === state.currentId ? " active" : "");
    item.innerHTML = `<svg viewBox="0 0 24 24">${ICON_CHAT}</svg>`;

    const name = document.createElement("span");
    name.className = "name";
    name.textContent = m.title || "Без названия";
    item.appendChild(name);

    const del = document.createElement("button");
    del.className = "conv-del";
    del.textContent = "×";
    del.title = "Удалить диалог";
    del.addEventListener("click", (e) => {
      e.stopPropagation();
      deleteConversation(m.id);
    });
    item.appendChild(del);

    item.addEventListener("click", () => openConversation(m.id));
    makePressable(item); // открывается и с клавиатуры (Tab + Enter)
    convListEl.appendChild(item);
  }
}

// Список чатов конкретного проекта (внутри экрана проекта).
export function renderProjectChats(projectId: string) {
  projectChatListEl.innerHTML = "";
  const items = convMetas.filter((m) => m.project_id === projectId);
  if (items.length === 0) {
    const empty = document.createElement("div");
    empty.className = "project-empty";
    empty.textContent = "В проекте пока нет чатов. Начните новый — он будет искать по документам проекта.";
    projectChatListEl.appendChild(empty);
    return;
  }
  for (const m of items) {
    const item = document.createElement("div");
    item.className = "conv";
    item.innerHTML = `<svg viewBox="0 0 24 24">${ICON_CHAT}</svg>`;
    const name = document.createElement("span");
    name.className = "name";
    name.textContent = m.title || "Без названия";
    item.appendChild(name);

    const del = document.createElement("button");
    del.className = "conv-del";
    del.textContent = "×";
    del.title = "Удалить чат";
    del.addEventListener("click", (e) => {
      e.stopPropagation();
      deleteConversation(m.id);
    });
    item.appendChild(del);

    item.addEventListener("click", () => openConversation(m.id));
    makePressable(item); // открывается и с клавиатуры (Tab + Enter)
    projectChatListEl.appendChild(item);
  }
}

// Индикатор в шапке: к какому проекту относится ОТКРЫТЫЙ чат (клик — открыть проект).
export function updateChatContextChip() {
  const proj = state.currentProjectId
    ? state.projects.find((p) => p.id === state.currentProjectId)
    : null;
  if (proj) {
    chatProjectChip.hidden = false;
    chatProjectChipName.textContent = proj.name;
  } else {
    chatProjectChip.hidden = true;
  }
}

// Поколение открытия: быстрые клики по списку запускают параллельные
// load_conversation, применяем только результат последнего клика — иначе
// победителя определяла бы скорость чтения с диска, а не пользователь.
let openGen = 0;

// Открывает диалог из файла в ленту. Восстанавливает и проект чата (для RAG/инструкций).
export async function openConversation(id: string) {
  if (state.streaming) stop();
  showChatView(); // вышли из настроек/экрана проекта — показываем ленту
  const my = ++openGen;
  let conv: Conversation;
  try {
    conv = await invoke<Conversation>("load_conversation", { id });
  } catch (e) {
    if (my === openGen) addError(`Не удалось открыть диалог: ${humanError(e)}`);
    return;
  }
  if (my !== openGen) return; // пока читали файл, пользователь выбрал другой диалог
  state.currentId = conv.id;
  state.currentProjectId = conv.project_id ?? null;
  history.length = 0;
  history.push(...conv.messages);
  renderHistory();
  updateChatContextChip();
  refreshCurrentDocsCount(); // база знаний могла быть у проекта этого чата
  refreshConversationList();
  inputEl.focus();
}

// «Новый чат»: пустой чат в указанном проекте (projectId=null — быстрый чат вне
// проектов). Старый уже сохранён — ничего не теряется.
export function newDialog(projectId: string | null = null) {
  if (state.streaming) stop();
  showChatView(); // вышли из настроек/экрана проекта — показываем ленту
  openGen++; // недогруженное открытие старого диалога не должно перекрыть новый чат
  state.currentId = crypto.randomUUID();
  state.currentProjectId = projectId;
  history.length = 0;
  shownSourceFiles.clear(); // новый диалог — источники снова показываем с первого раза
  messagesEl.innerHTML = "";
  refreshEmptyState(); // пустой диалог → показываем приветствие
  updateChatContextChip();
  refreshCurrentDocsCount(); // у проекта новый чат сразу видит его базу знаний
  refreshConversationList(); // снимет подсветку (нового ещё нет в списке)
  inputEl.focus();
}

// Удаление диалога (с подтверждением — потеря данных необратима).
export async function deleteConversation(id: string) {
  if (!(await confirmModal("Удалить этот диалог? Действие необратимо."))) return;
  // Помечаем ДО удаления файла и дожидаемся очереди записей: автосейв идущей
  // генерации иначе мог бы записать файл заново уже после удаления.
  deletedIds.add(id);
  await saveChain.catch(() => {});
  try {
    await invoke("delete_conversation", { id });
  } catch (e) {
    deletedIds.delete(id); // файл не удалён — записи по этому id снова разрешены
    addError(`Не удалось удалить диалог: ${humanError(e)}`);
    return;
  }
  if (id === state.currentId) {
    newDialog(state.currentProjectId);
  } else {
    await refreshConversationList();
  }
  if (!projectView.hidden && state.viewingProjectId) renderProjectChats(state.viewingProjectId);
}

// Очистка всех диалогов (вся история стирается с диска).
async function clearAllConversations() {
  if (
    !(await confirmModal(
      "Удалить ВСЕ диалоги? Вся история будет стёрта безвозвратно.",
      "Удалить всё",
    ))
  )
    return;
  // Текущий диалог мог стримиться: помечаем его удалённым и дожидаемся очереди
  // записей, чтобы дочистка «Стопа» не воскресила один файл после очистки всех.
  const oldId = state.currentId;
  if (oldId) deletedIds.add(oldId);
  await saveChain.catch(() => {});
  try {
    await invoke("clear_conversations");
  } catch (e) {
    if (oldId) deletedIds.delete(oldId);
    addError(`Не удалось очистить диалоги: ${humanError(e)}`);
    return;
  }
  newDialog(); // начинаем с чистого листа
}

// При старте: открыть самый свежий диалог или начать пустой.
export async function initConversations() {
  let metas: ConversationMeta[] = [];
  try {
    metas = await invoke<ConversationMeta[]>("list_conversations");
  } catch {
    metas = [];
  }
  if (metas.length > 0) {
    await openConversation(metas[0].id); // свежий сверху
  }
  // Нет диалогов ИЛИ не удалось открыть (currentId не выставился) — начинаем новый,
  // иначе следующие сообщения молча не сохранятся (persist требует currentId).
  if (!state.currentId) {
    state.currentId = crypto.randomUUID();
    await refreshConversationList();
  }
  updateChatContextChip(); // показать чип проекта, если открылся чат внутри проекта
  refreshEmptyState(); // история загружена — теперь решаем, показывать ли приветствие
}

// Обработчики: новый чат, поиск по списку, очистка истории, горячие клавиши.
export function wireConversations() {
  newChatBtn.addEventListener("click", () => newDialog(null));
  convSearchEl.addEventListener("input", () => {
    convFilter = convSearchEl.value;
    renderConvList();
  });
  clearChatsBtn.addEventListener("click", clearAllConversations); // кнопка в настройках («История диалогов»)

  // Глобальные клавиши: Esc — «Стоп» во время генерации (в модальном окне Esc
  // остаётся отменой окна); Ctrl/Cmd+N — новый быстрый чат. По e.code, чтобы
  // работало и в русской раскладке.
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && state.streaming && !document.querySelector(".modal-overlay")) {
      stop();
    } else if ((e.ctrlKey || e.metaKey) && !e.altKey && !e.shiftKey && e.code === "KeyN") {
      e.preventDefault();
      newDialog(null);
    }
  });
}
