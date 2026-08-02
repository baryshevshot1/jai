// Диктовка: кнопка микрофона в композере. Пользователь наговаривает вопрос вместо
// набора — распознанный текст дописывается в поле ввода, отправляет он его сам.
//
// Способ записи — удержание («нажми и говори»). Тумблер «включил-выключил» удобнее
// пальцам, но оставляет микрофон открытым, когда о нём забыли. Здесь запись идёт
// ровно столько, сколько кнопку держат: для офлайнового продукта видимая граница
// «микрофон открыт / микрофон закрыт» — часть обещания о приватности, а не мелочь
// оформления. Само распознавание тоже локальное (src-tauri/src/voice.rs).

import { invoke } from "@tauri-apps/api/core";
import { autoGrow } from "./chat";
import { inputEl, voiceBtn, voiceStateEl } from "./dom";
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

const IDLE_TITLE = "Продиктовать вопрос: нажмите и удерживайте, говорите, отпустите";
const REC_TITLE = "Идёт запись — отпустите, чтобы распознать";

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
  followComposerState();

  voiceBtn.addEventListener("pointerdown", (e) => {
    e.preventDefault(); // не забирать фокус у поля: текст всё равно поедет туда
    void startDictation();
  });
  // Отпускают кнопку часто мимо неё — курсор или палец уезжает за край. Слушаем
  // окно, иначе запись осталась бы включённой, а пользователь считал бы, что нет.
  window.addEventListener("pointerup", () => void stopDictation());
  window.addEventListener("pointercancel", () => void stopDictation());

  // Клавиатура: удержание пробела или Enter на сфокусированной кнопке.
  voiceBtn.addEventListener("keydown", (e) => {
    if (e.key !== " " && e.key !== "Enter") return;
    // Пробел иначе прокручивает страницу, а Enter — «нажимает» кнопку сам;
    // автоповтор клавиши приходит сюда же, повторный запуск отсекает этап.
    e.preventDefault();
    if (e.repeat) return;
    void startDictation();
  });
  // keyup слушаем на окне: пока клавишу держали, фокус мог уйти с кнопки, и
  // событие до неё уже не дойдёт — запись «залипла» бы включённой.
  window.addEventListener("keyup", (e) => {
    if (e.key === " " || e.key === "Enter") void stopDictation();
  });
  // Переключение на другое окно (Alt+Tab) не доставит ни pointerup, ни keyup —
  // закрываем запись сами: открытый в фоне микрофон недопустим.
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

async function startDictation(): Promise<void> {
  // Диктовать нечего поверх идущего ответа: модель занята, а поле ввода выключено.
  if (phase !== "idle" || voiceBtn.hidden || voiceBtn.disabled || state.streaming) return;
  phase = "recording";
  render();
  starting = invoke<void>("voice_start").then(
    () => true,
    (e) => {
      // Микрофона нет или система не дала к нему доступ — сказать и выйти из режима.
      if (phase === "recording") {
        phase = "idle";
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
  voiceBtn.setAttribute("aria-pressed", String(rec));
  voiceBtn.title = rec ? REC_TITLE : IDLE_TITLE;
  voiceStateEl.hidden = phase === "idle";
  voiceStateEl.textContent = rec ? "Говорите…" : phase === "decoding" ? "Распознаю…" : "";
  voiceStateEl.classList.toggle("voice-state--rec", rec);
  voiceStateEl.classList.toggle("voice-state--busy", phase === "decoding");
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
