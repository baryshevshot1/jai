// Единый менеджер установки моделей (фронтенд-половина; бэкенд — PullState в lib.rs).
// Все три поверхности установки (карточка bge-m3 в сайдбаре, каталог моделей в
// настройках, предложение vision-модели в чате) идут через runPull: канал прогресса
// + честный итог по результату команды (done/cancelled) — «установлено» больше не
// показывается после отмены.

import { Channel, invoke } from "@tauri-apps/api/core";
import type { PullEvent, PullOutcome } from "./types";
import { state } from "./state";
import {
  installEmbedBtn,
  installFromDiskBtn,
  installLocalBtn,
  modelListEl,
  modelPullCancelBtn,
  pullCancelBtn,
} from "./dom";
import { gb, humanError, ruPullStatus } from "./util";

// Адаптер поверхности установки: как рисовать прогресс и итог в её виджетах.
export interface PullUi {
  progress(text: string, frac: number): void;
  done(): void;
  cancelled(): void;
  error(msg: string): void; // msg уже прогнан через humanError
}

// Заблокировать/разблокировать все точки входа установки на время pull: повторный
// вход и параллельные установки запрещены (бэкенд тоже отклонит — здесь мгновенный
// локальный гейт). Кнопки строк каталога пересоздаёт renderModelList — он сам
// смотрит на state.pullingTag при отрисовке.
function setPullButtonsEnabled(on: boolean): void {
  installEmbedBtn.disabled = !on;
  installLocalBtn.disabled = !on;
  installFromDiskBtn.disabled = !on;
  modelListEl.querySelectorAll<HTMLButtonElement>("button").forEach((b) => (b.disabled = !on));
}

// Единый поток установки модели.
// Общее ядро операций установки (скачивание из интернета / импорт с носителя):
// гейт от параллельных операций, канал прогресса, честный итог done/cancelled.
async function runInstallOp(
  label: string,
  exec: (onEvent: Channel<PullEvent>) => Promise<PullOutcome>,
  ui: PullUi,
): Promise<PullOutcome | "error"> {
  if (state.pullingTag) {
    ui.error(`Уже идёт установка «${state.pullingTag}» — дождитесь завершения или отмените её.`);
    return "error";
  }
  state.pullingTag = label;
  setPullButtonsEnabled(false);
  // Канал и результат команды — разные пути доставки IPC: после итога гасим
  // запоздавшие progress-сообщения, чтобы они не затирали финальную надпись.
  let settled = false;
  const onEvent = new Channel<PullEvent>();
  onEvent.onmessage = (e) => {
    if (settled) return;
    const frac = e.total > 0 ? e.completed / e.total : 0;
    const tail =
      e.total > 0 ? ` ${Math.round(frac * 100)}% (${gb(e.completed)} из ${gb(e.total)})` : "";
    ui.progress(`${ruPullStatus(e.status)}${tail}`, frac);
  };
  try {
    const outcome = await exec(onEvent);
    settled = true;
    if (outcome === "cancelled") ui.cancelled();
    else ui.done();
    return outcome;
  } catch (e) {
    settled = true;
    ui.error(humanError(e));
    return "error";
  } finally {
    state.pullingTag = null;
    setPullButtonsEnabled(true);
  }
}

// Скачивание модели из интернета (Ollama /api/pull).
export async function runPull(tag: string, ui: PullUi): Promise<PullOutcome | "error"> {
  return runInstallOp(
    tag,
    (onEvent) => invoke<PullOutcome>("pull_model", { name: tag, onEvent }),
    ui,
  );
}

// Импорт моделей с флешки/диска: копирование в каталог Ollama на компьютере
// (после импорта носитель можно вынуть). Отмена — тем же cancel_pull.
export async function runImport(dir: string, ui: PullUi): Promise<PullOutcome | "error"> {
  return runInstallOp(
    "импорт моделей с диска",
    (onEvent) => invoke<PullOutcome>("import_models_from_dir", { path: dir, onEvent }),
    ui,
  );
}

// Отмена активной установки — кнопки всех поверхностей ведут сюда. Итог придёт
// результатом pull_model, и поверхность отреагирует своей веткой cancelled.
export function cancelActivePull() {
  pullCancelBtn.disabled = true;
  modelPullCancelBtn.disabled = true;
  invoke("cancel_pull").catch(() => {});
}
