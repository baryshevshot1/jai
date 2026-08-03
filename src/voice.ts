// Диктовка: кнопка микрофона в композере. Пользователь наговаривает вопрос вместо
// набора — распознанный текст дописывается в поле ввода, отправляет он его сам.
//
// Способов записи два, и выбирает их сам жест: удержание — для короткой фразы,
// короткое нажатие защёлкивает запись — для длинной. Подробнее у TAP_MS ниже.
//
// Чистый тумблер «включил-выключил» был бы удобнее, но оставляет микрофон открытым,
// когда о нём забыли, а для офлайнового продукта видимая граница «микрофон открыт /
// закрыт» — часть обещания о приватности, а не мелочь оформления. Поэтому у защёлки
// есть счётчик времени на виду и потолок длительности, на котором запись
// закрывается сама. Само распознавание тоже локальное (src-tauri/src/voice.rs).

import { invoke } from "@tauri-apps/api/core";
import { autoGrow } from "./chat";
import { inputEl, voiceAnnounceEl, voiceBtn, voiceStateEl } from "./dom";
import { state } from "./state";
import { addError, addNotice } from "./ui";
import { humanError } from "./util";

// Этапы одной диктовки. «Распознаю» существует отдельно от «идёт запись», потому
// что длится секунды на процессоре: без отдельного этапа отпущенная кнопка
// выглядела бы так, будто нажатие потерялось, и пользователь жал бы ещё раз.
type Phase = "idle" | "recording" | "decoding";
let phase: Phase = "idle";

// Обещание запуска записи. Короткое нажатие («тык») отпускают раньше, чем движок
// успевает открыть микрофон, — стоп обязан дождаться старта, иначе бэкенд получит
// «остановить» до «начать» и ответит отказом на пустом месте.
let starting: Promise<boolean> | null = null;

// Обработчики уже навешаны. Нужен потому, что initVoice зовут повторно — после
// установки модели, чтобы кнопка появилась без перезапуска приложения.
let wired = false;

// ── Два способа диктовать одной кнопкой ──────────────────────────────────────
//
// Удержание («зажал — говорю — отпустил») хорошо для короткой фразы и плохо для
// длинной: держать палец полминуты неудобно, а отпустить случайно — легко.
// Тумблер («нажал — говорю — нажал») удобен для длинного, но оставляет микрофон
// открытым, когда о нём забыли, а для офлайнового продукта видимая граница
// «микрофон открыт / закрыт» — часть обещания о приватности, а не мелочь.
//
// Поэтому кнопка одна, а способа два, и выбирает их сам жест:
//   • отпустили быстро (тык)  → запись ЗАЩЁЛКИВАЕТСЯ и идёт до второго нажатия;
//   • держите дольше порога   → работает как раньше, отпускание завершает.
//
// Обещание о приватности защёлка не нарушает: пока она включена, идёт видимый
// счётчик времени, а на потолке длительности запись закрывается сама.
const TAP_MS = 350;

// Потолок одной диктовки. ДОЛЖЕН совпадать с MAX_RECORDING_SECS в
// src-tauri/src/voice.rs — там на нём обрывается накопление буфера. Разъедутся:
// либо счётчик врёт, либо запись «идёт», а звук уже не пишется. Совпадение
// проверяет тест voice::tests::recording_cap_matches_frontend.
const MAX_RECORDING_SECS = 120;

// Запись идёт без удержания кнопки (защёлкнута тыком).
let latched = false;
// Когда нажали — чтобы отличить тык от удержания.
let pressedAt = 0;
// Начало записи — для счётчика времени.
let startedAt = 0;
// Таймер, который обновляет счётчик и закрывает микрофон на потолке.
let ticker: number | null = null;

const IDLE_TITLE = "Продиктовать вопрос: нажмите и удерживайте — либо нажмите коротко, чтобы говорить без удержания";
const HOLD_TITLE = "Идёт запись — отпустите, чтобы распознать";
const LATCHED_TITLE = "Идёт запись — нажмите ещё раз, чтобы закончить";

// Готовит диктовку к работе: показывает кнопку, только если модель распознавания
// установлена. Нерабочая кнопка хуже отсутствующей — нажатие с ошибкой в ответ
// выглядит как поломка приложения, а не как «эта возможность не поставлена».
export async function initVoice(): Promise<void> {
  let ready = false;
  try {
    ready = await invoke<boolean>("voice_available");
  } catch {
    ready = false; // сборка без голосового ввода — живём дальше, просто без кнопки
  }
  voiceBtn.hidden = !ready;
  if (!ready) return; // кнопки нет — и обработчики ни к чему

  render();
  // Второй раз сюда заходят после установки модели из настроек — чтобы кнопка
  // появилась без перезапуска приложения. Обработчики при этом навешивать заново
  // нельзя: они на окне, снять их некому, и каждая установка удваивала бы реакцию
  // на одно нажатие.
  if (wired) return;
  wired = true;

  followComposerState();

  voiceBtn.addEventListener("pointerdown", (e) => {
    e.preventDefault(); // не забирать фокус у поля: текст всё равно поедет туда
    void press();
  });
  // Отпускают кнопку часто мимо неё — курсор или палец уезжает за край. Слушаем
  // окно, иначе запись осталась бы включённой, а пользователь считал бы, что нет.
  window.addEventListener("pointerup", () => void release());
  window.addEventListener("pointercancel", () => void release());

  // Клавиатура: тот же жест — короткое нажатие защёлкивает, удержание работает
  // как удержание. Пробел иначе прокручивает страницу, а Enter «нажимает» кнопку
  // сам; автоповтор клавиши приходит сюда же и отсекается по e.repeat.
  voiceBtn.addEventListener("keydown", (e) => {
    if (e.key !== " " && e.key !== "Enter") return;
    e.preventDefault();
    if (e.repeat) return;
    void press();
  });
  // keyup слушаем на окне: пока клавишу держали, фокус мог уйти с кнопки, и
  // событие до неё уже не дойдёт — запись «залипла» бы включённой.
  window.addEventListener("keyup", (e) => {
    if (e.key === " " || e.key === "Enter") void release();
  });
  // Escape — «передумал»: микрофон закрываем, сказанное выбрасываем. Без этого
  // единственным способом прекратить начатую диктовку было договорить её до конца
  // и потом стирать распознанное руками.
  window.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && phase === "recording") {
      e.preventDefault();
      void cancelDictation();
    }
  });
  // Переключение на другое окно (Alt+Tab) не доставит ни pointerup, ни keyup —
  // закрываем запись сами: открытый в фоне микрофон недопустим.
  //
  // Именно «стоп», а не отмена: уход из окна — намерение неявное (мог мигнуть
  // системный уведомитель), и выбрасывать за него уже сказанное значит молча терять
  // работу. Текст попадёт в поле, стереть его — одно движение. Выбрасывает только
  // Escape, потому что это явное «передумал».
  window.addEventListener("blur", () => void stopDictation());
}

// Дописывает распознанное к уже набранному: пользователь мог начать печатать и
// продолжить голосом — затирать начатое нельзя. Разделитель ставим сами, но не
// удваиваем уже стоящий (иначе после переноса строки появлялся бы висячий пробел).
export function joinDictated(current: string, dictated: string): string {
  const add = dictated.trim();
  if (!add) return current;
  if (!current) return add;
  return /\s$/.test(current) ? current + add : `${current} ${add}`;
}

// Нажатие. Если запись уже защёлкнута — это второе нажатие, оно её завершает.
async function press(): Promise<void> {
  if (phase === "recording" && latched) {
    await stopDictation();
    return;
  }
  pressedAt = Date.now();
  await startDictation();
}

// Отпускание. Быстрое («тык») переводит запись в режим без удержания; долгое
// завершает её, как раньше. Порог сравниваем с моментом НАЖАТИЯ, а не с началом
// записи: движок открывает микрофон не мгновенно, и по нему тык выглядел бы
// удержанием.
async function release(): Promise<void> {
  if (phase !== "recording" || latched) return;
  if (Date.now() - pressedAt < TAP_MS) {
    latched = true;
    render();
    return;
  }
  await stopDictation();
}

// Счётчик времени записи. Он же — сторож потолка: на MAX_RECORDING_SECS запись
// закрывается сама. Без этого защёлка могла бы держать микрофон открытым, пока о
// нём не вспомнят, а обещание «видно, когда микрофон открыт» стало бы формальным.
function startTicker(): void {
  stopTicker();
  startedAt = Date.now();
  ticker = window.setInterval(() => {
    if (phase !== "recording") return;
    // Пятнадцать секунд до потолка — это СОБЫТИЕ, а не тик счётчика: его диктору
    // сказать надо, и ровно один раз.
    if (MAX_RECORDING_SECS - elapsedSecs() <= 15) {
      announce("Запись скоро закончится");
    }
    if (elapsedSecs() >= MAX_RECORDING_SECS) {
      addNotice(
        `Достигнут предел одной диктовки (${Math.round(MAX_RECORDING_SECS / 60)} мин) — ` +
          "запись закрыта, распознаю сказанное.",
      );
      void stopDictation();
      return;
    }
    render();
  }, 250);
}

function stopTicker(): void {
  if (ticker !== null) {
    window.clearInterval(ticker);
    ticker = null;
  }
}

function elapsedSecs(): number {
  return startedAt ? Math.floor((Date.now() - startedAt) / 1000) : 0;
}

// Время в подписи: «0:07». Секунда — достаточная точность, а вид «7 секунд»
// прыгал бы по ширине и дёргал строку.
export function clock(secs: number): string {
  const m = Math.floor(secs / 60);
  const s = secs % 60;
  return `${m}:${s < 10 ? "0" : ""}${s}`;
}

async function startDictation(): Promise<void> {
  // Диктовать нечего поверх идущего ответа: модель занята, а поле ввода выключено.
  if (phase !== "idle" || voiceBtn.hidden || voiceBtn.disabled || state.streaming) return;
  phase = "recording";
  latched = false;
  startTicker();
  render();
  starting = invoke<void>("voice_start").then(
    () => true,
    (e) => {
      // Микрофона нет или система не дала к нему доступ — сказать и выйти из режима.
      if (phase === "recording") {
        phase = "idle";
        latched = false;
        stopTicker();
        render();
      }
      addError(humanError(e));
      return false;
    },
  );
}

async function stopDictation(): Promise<void> {
  if (phase !== "recording") return; // не начинали — или уже распознаём
  phase = "decoding";
  latched = false;
  stopTicker();
  render();
  const started = await (starting ?? Promise.resolve(false));
  starting = null;
  if (!started) {
    phase = "idle"; // причину неудачного запуска уже показали
    render();
    return;
  }
  try {
    const text = await invoke<string>("voice_stop");
    if (text.trim()) insertDictated(text);
    // Пустой ответ — не поломка: в записи просто не нашлось речи. Ошибкой такое
    // показывать нечестно, человек ничего не сделал не так.
    else addNotice("Речь не разобрана — скажите фразу погромче и ближе к микрофону.");
  } catch (e) {
    addError(humanError(e));
  } finally {
    phase = "idle";
    render();
  }
}

// Отмена: закрыть микрофон и выбросить записанное. Дожидаемся обещания запуска —
// иначе отмена может прийти раньше, чем движок успел открыть микрофон, и запись
// осталась бы включённой уже после того, как её отменили.
async function cancelDictation(): Promise<void> {
  if (phase !== "recording") return;
  phase = "idle";
  latched = false;
  stopTicker();
  render();
  const pending = starting;
  starting = null;
  try {
    if (pending) await pending;
    await invoke("voice_cancel");
  } catch {
    /* нечего отменять либо сборка без голоса — состояние уже сброшено */
  }
}

function insertDictated(text: string): void {
  const merged = joinDictated(inputEl.value, text);
  inputEl.value = merged;
  autoGrow(); // поле подрастает под надиктованное так же, как при наборе
  inputEl.focus();
  inputEl.setSelectionRange(merged.length, merged.length); // курсор — в конец
}

function render(): void {
  const rec = phase === "recording";
  voiceBtn.classList.toggle("is-recording", rec);
  // Открытый микрофон подсвечивает ВСЮ карточку композера, а не только кнопку:
  // заметить рамку вокруг поля ввода можно боковым зрением, мелкую кнопку в углу —
  // нет. Для офлайнового продукта это часть обещания о приватности.
  voiceBtn.closest(".composer")?.classList.toggle("is-recording", rec);
  // Защёлкнутая запись выглядит ИНАЧЕ, а не так же: пока микрофон держат пальцем,
  // человек про него помнит; как только держать перестали — напоминать должен экран.
  voiceBtn.classList.toggle("is-latched", rec && latched);
  voiceBtn.setAttribute("aria-pressed", String(rec));
  voiceBtn.title = rec ? (latched ? LATCHED_TITLE : HOLD_TITLE) : IDLE_TITLE;
  voiceStateEl.hidden = phase === "idle";
  // Пишем во ВЛОЖЕННЫЙ span, а не в сам элемент: многоточие при нехватке места
  // умеет только он, а textContent на родителе снёс бы его вместе с разметкой.
  const label = voiceStateEl.querySelector(".voice-state__text") ?? voiceStateEl;
  label.textContent = rec ? recordingLabel() : phase === "decoding" ? "Распознаю…" : "";
  announce(rec ? (latched ? "Идёт запись без удержания" : "Идёт запись") : phase === "decoding" ? "Распознаю" : "");
  voiceStateEl.classList.toggle("voice-state--rec", rec);
  voiceStateEl.classList.toggle("voice-state--busy", phase === "decoding");
}

// Сколько секунд держать подсказку «как закончить» после защёлкивания. Прочитать
// её нужно один раз; дальше она только шумит, а роль «микрофон открыт» берут на
// себя счётчик и вид самой кнопки. Восемь секунд — с запасом на прочитать не
// торопясь: место под подпись теперь есть, ограничивает только внимание.
const HINT_SECS = 8;

// Последнее, что уже сказано диктору: повторять то же самое нельзя — каждая запись
// в живую область перебивает предыдущую.
let announced = "";

// Сообщить экранному диктору смену этапа. Бегущее время сюда НЕ попадает: оно
// меняется четыре раза в секунду, и диктор всю диктовку перебивал бы сам себя,
// заглушая заодно и предупреждение о скором обрыве записи.
function announce(text: string): void {
  if (text === announced) return;
  announced = text;
  voiceAnnounceEl.textContent = text;
}

// Подпись во время записи. Время показываем всегда: это и обратная связь для
// длинной диктовки, и — главное — постоянное напоминание, что микрофон открыт.
export function recordingLabel(secs = elapsedSecs(), isLatched = latched): string {
  const left = MAX_RECORDING_SECS - secs;
  // На последних секундах предупреждаем: запись оборвётся сама, и лучше узнать об
  // этом заранее, чем на полуслове. Это важнее всего остального — показываем вместо.
  if (left <= 15) return `Осталось ${left} с`;
  if (isLatched) {
    return secs <= HINT_SECS ? `Запись ${clock(secs)} — нажмите, чтобы закончить` : `Запись ${clock(secs)}`;
  }
  return `Говорите… ${clock(secs)}`;
}

// Диктовка живёт по тем же правилам, что и набор: пока модель отвечает и пока
// отвечать нечем, поле ввода выключено — значит, и микрофон недоступен. Отдельного
// сигнала об этом нет, поэтому следим за самим полем: одно условие вместо второй
// его копии, которая рано или поздно разойдётся с первой.
function followComposerState(): void {
  const sync = () => {
    voiceBtn.disabled = inputEl.disabled;
    // Ответ начался прямо во время диктовки — закрываем микрофон, но сказанное
    // не выбрасываем: распознаём и дописываем в поле, как при обычном отпускании.
    if (inputEl.disabled) void stopDictation();
  };
  new MutationObserver(sync).observe(inputEl, {
    attributes: true,
    attributeFilter: ["disabled"],
  });
  sync();
}
