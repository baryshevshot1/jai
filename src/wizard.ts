// Мастер установки: открывается сам при первом запуске (обязательные модели не
// установлены) и по кнопке из настроек. Оценивает машину, предлагает ПОСИЛЬНЫЕ
// модели набора (честные вердикты «потянет/впритык/не потянет» — та же формула
// памяти, что защищает от свопа) и ставит выбранное с флешки (импорт) или из
// интернета. Вся логика — готовые команды provision/pull; мастер только интерфейс.

import { invoke } from "@tauri-apps/api/core";
import type { HardwareInfo, ModelAssessment, ModelSource } from "./types";
import {
  openWizardBtn,
  wizardCard,
  wizardCloseBtn,
} from "./dom";
import { humanError, plural } from "./util";
import { showChatView, showStatus, type StatusKind, showScreen} from "./ui";
import { cancelActivePull, runImport, runPull } from "./pull";
import { loadModels, loadModelStates } from "./models";
import { refreshCurrentDocsCount, refreshDocuments, sidebarDocCtx } from "./documents";

// ГБ с одним знаком; «~» — размер приблизительный (модель ещё не установлена).
const fmtGb = (g: number, approx = false) => `${approx ? "~" : ""}${g.toFixed(1)} ГБ`;

// Короткий помощник создания элементов (мастер строит содержимое динамически).
function el<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  cls?: string,
  text?: string,
): HTMLElementTagNameMap[K] {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text !== undefined) e.textContent = text;
  return e;
}

export async function openWizard() {
  // Мастер занимает место ленты (как настройки/экран проекта) и взаимоисключаем с ними.
  showScreen("wizard");
  await renderSetupScreen();
}

function closeWizard() {
  showChatView();
  refreshDocuments(sidebarDocCtx);
  refreshCurrentDocsCount();
}

// Первый запуск: обязательные модели набора не установлены → мастер открывается сам.
// Движок молчит → тихо пропускаем (мастер остаётся доступен кнопкой в настройках).
export async function maybeOfferWizard() {
  try {
    const res = await invoke<{ models: { required: boolean; installed: boolean }[] }>(
      "model_states",
    );
    if (res.models.some((m) => m.required && !m.installed)) await openWizard();
  } catch {
    /* нет движка/моделей не узнать — не мешаем обычному запуску */
  }
}

// Бейдж вердикта (стили бейджей моделей переиспользуются).
function verdictBadge(a: ModelAssessment): { text: string; cls: string } {
  if (a.installed) return { text: "установлена", cls: "model-badge--ok" };
  if (a.verdict === "ok") return { text: "потянет", cls: "model-badge--ok" };
  if (a.verdict === "tight") return { text: "впритык", cls: "model-badge--update" };
  return { text: "не потянет", cls: "model-badge--err" };
}

// ── Экран подбора: машина → модели → источник ────────────────────────────────

async function renderSetupScreen() {
  wizardCard.innerHTML = "";
  const selected = new Set<string>();

  wizardCard.appendChild(el("h3", "wizard-h", "Ваш компьютер"));
  const hwLine = el("div", "wizard-note", "Определяю железо…");
  wizardCard.appendChild(hwLine);
  // Помехи (железо не определилось, движок молчит) — отдельной заметной строкой:
  // в общем сером перечислении характеристик их просто не замечают.
  const hwTrouble = el("div");
  hwTrouble.hidden = true;
  wizardCard.appendChild(hwTrouble);

  wizardCard.appendChild(el("h3", "wizard-h", "Модели"));
  wizardCard.appendChild(
    el(
      "div",
      "wizard-note",
      "Отмечены рекомендуемые для этой машины. «Не потянет» — честная оценка памяти: такие модели здесь лучше не ставить.",
    ),
  );
  const listEl = el("div", "model-list");
  wizardCard.appendChild(listEl);
  const totalLine = el("div", "wizard-total", "");
  wizardCard.appendChild(totalLine);

  wizardCard.appendChild(el("h3", "wizard-h", "Откуда установить"));
  const srcList = el("div", "wizard-sources");
  wizardCard.appendChild(srcList);
  const srcRefresh = el("button", "ep-btn ep-btn--ghost", "Обновить список носителей");
  srcRefresh.type = "button";
  wizardCard.appendChild(srcRefresh);

  const actions = el("div", "wizard-actions");
  const installBtn = el("button", "ep-btn ep-btn--primary", "Установить выбранные");
  installBtn.type = "button";
  const laterBtn = el("button", "ep-btn ep-btn--ghost", "Позже");
  laterBtn.type = "button";
  actions.append(installBtn, laterBtn);
  wizardCard.appendChild(actions);
  const status = el("div");
  status.hidden = true;
  wizardCard.appendChild(status);

  laterBtn.addEventListener("click", closeWizard);

  // Железо и движок — параллельно, не задерживая список моделей.
  void (async () => {
    const parts: string[] = [];
    const trouble: string[] = [];
    try {
      const hw = await invoke<HardwareInfo>("detect_hardware");
      const word =
        hw.tier === "green" ? "Оптимально" : hw.tier === "yellow" ? "Достаточно" : "Ограничено";
      parts.push(word);
      if (hw.vram_gb != null) {
        const free = hw.vram_free_gb != null ? ` (свободно ${hw.vram_free_gb.toFixed(1)})` : "";
        parts.push(`GPU ${hw.vram_gb.toFixed(0)} ГБ${free}`);
      }
      parts.push(`ОЗУ ${hw.ram_gb.toFixed(0)} ГБ`);
      parts.push(`CPU ${hw.cpu_cores} ${plural(hw.cpu_cores, "ядро", "ядра", "ядер")}`);
    } catch {
      trouble.push(
        "Определить характеристики компьютера не удалось — оценки моделей ниже приблизительные.",
      );
    }
    try {
      const v = await invoke<string>("ollama_version");
      parts.push(`движок Ollama ${v}`);
    } catch {
      trouble.push(
        "Движок не отвечает: скачать модели из интернета не получится. Импорт с флешки при этом работает.",
      );
    }
    hwLine.textContent = parts.length ? parts.join(" · ") : "Характеристики компьютера неизвестны";
    if (trouble.length) showStatus(hwTrouble, trouble.join(" "), "error");
  })();

  // Оценка набора против железа (работает и без движка — по приблизительным весам).
  let assessments: ModelAssessment[] = [];
  try {
    assessments = await invoke<ModelAssessment[]>("assess_models");
  } catch (e) {
    // Пустой список без объяснения выглядел бы как «моделей не нашлось».
    const box = el("div");
    listEl.appendChild(box);
    showStatus(
      box,
      `Не удалось получить набор моделей: ${humanError(e)}. Закройте мастер и откройте ` +
        "заново — или установите модели в настройках (раздел «Модели»).",
      "error",
    );
  }

  // Рекомендация: лучшая посильная текстовая (набор упорядочен от основной к лёгкой)
  // + обязательный поиск по документам (bge-m3), если машина его тянет.
  const textPick =
    assessments.find((a) => a.role === "text" && !a.installed && a.verdict === "ok") ??
    assessments.find((a) => a.role === "text" && !a.installed && a.verdict === "tight");
  const embedPick = assessments.find(
    (a) => a.role === "embed" && !a.installed && a.verdict !== "no",
  );
  const recommended = new Set(
    [textPick?.tag, embedPick?.tag].filter((t): t is string => Boolean(t)),
  );

  const refreshTotal = () => {
    const sum = assessments
      .filter((a) => selected.has(a.tag))
      .reduce((s, a) => s + a.size_gb, 0);
    const approx = assessments.some((a) => selected.has(a.tag) && a.approx);
    totalLine.textContent = selected.size
      ? `Выбрано: ${selected.size} · ${fmtGb(sum, approx)} на диске`
      : "Ничего не выбрано";
  };

  for (const a of assessments) {
    const row = el("label", "model-row wizard-model");
    const cb = el("input");
    cb.type = "checkbox";
    cb.disabled = a.installed || a.verdict === "no";
    if (!a.installed && recommended.has(a.tag)) {
      cb.checked = true;
      selected.add(a.tag);
    }
    cb.addEventListener("change", () => {
      if (cb.checked) selected.add(a.tag);
      else selected.delete(a.tag);
      refreshTotal();
    });
    row.appendChild(cb);

    const info = el("div", "model-row__info");
    const title = el("div", "model-row__title", a.title);
    if (a.required) title.appendChild(el("span", "model-row__req", "обязательная"));
    const sub = el("div", "model-row__tag", `${a.tag} · ${fmtGb(a.size_gb, a.approx)}`);
    info.append(title, sub);
    row.appendChild(info);

    const b = verdictBadge(a);
    row.appendChild(el("span", `model-badge ${b.cls}`, b.text));
    listEl.appendChild(row);
  }
  refreshTotal();

  // Источник: найденные носители (флешка — первым) + интернет.
  const renderSources = async () => {
    srcList.innerHTML = "";
    let sources: ModelSource[] = [];
    let srcTrouble = "";
    try {
      sources = await invoke<ModelSource[]>("find_model_sources");
    } catch (e) {
      // Промолчать нельзя: список без флешки читается как «носитель не вставлен».
      srcTrouble =
        `Проверить подключённые носители не удалось: ${humanError(e)}. ` +
        "Если флешка вставлена, нажмите «Обновить список носителей».";
    }
    const addOption = (value: string, label: string, sub: string, checked: boolean) => {
      const row = el("label", "wizard-source");
      const r = el("input");
      r.type = "radio";
      r.name = "wizard-src";
      r.value = value;
      r.checked = checked;
      row.appendChild(r);
      const box = el("div");
      box.appendChild(el("div", "", label));
      box.appendChild(el("div", "wizard-source__sub", sub));
      row.appendChild(box);
      srcList.appendChild(row);
    };
    sources.forEach((s, i) =>
      addOption(
        s.path,
        s.removable ? "С флешки (быстро, без интернета)" : "С диска (без интернета)",
        `${s.path} · моделей: ${s.models} · ${s.total_gb.toFixed(1)} ГБ`,
        i === 0,
      ),
    );
    addOption(
      "internet",
      "Из интернета",
      "модели скачиваются по очереди; нужен доступ в интернет",
      sources.length === 0,
    );
    if (srcTrouble) {
      const box = el("div");
      srcList.appendChild(box);
      showStatus(box, srcTrouble, "error");
    }
  };
  await renderSources();
  srcRefresh.addEventListener("click", () => void renderSources());

  installBtn.addEventListener("click", async () => {
    const chosen = assessments.filter((a) => selected.has(a.tag)).map((a) => a.tag);
    const src =
      srcList.querySelector<HTMLInputElement>('input[name="wizard-src"]:checked')?.value ??
      "internet";
    if (!chosen.length && src === "internet") {
      showStatus(status, "Отметьте хотя бы одну модель — иначе устанавливать нечего.", "error");
      return;
    }
    await renderInstallScreen(chosen, src, assessments);
  });
}

// ── Экран установки: прогресс по операциям → итог ────────────────────────────

async function renderInstallScreen(
  tags: string[],
  source: string,
  assessments: ModelAssessment[],
) {
  wizardCard.innerHTML = "";
  wizardCard.appendChild(el("h3", "wizard-h", "Установка"));
  const list = el("div");
  wizardCard.appendChild(list);
  const cancelBtn = el("button", "pull-cancel-btn", "Отмена");
  cancelBtn.type = "button";
  cancelBtn.addEventListener("click", () => {
    cancelBtn.disabled = true;
    cancelActivePull();
  });
  wizardCard.appendChild(cancelBtn);

  // Строка прогресса одной операции (разметка — как у прогресса индексации).
  const progressRow = (label: string) => {
    const wrap = el("div", "wizard-progress");
    wrap.appendChild(el("div", "wizard-progress__title", label));
    const bar = el("div", "index-progress__bar");
    const fill = el("i");
    bar.appendChild(fill);
    const lbl = el("div", "index-progress__label", "Подготовка…");
    wrap.append(bar, lbl);
    list.appendChild(wrap);
    return {
      progress: (t: string, f: number) => {
        fill.style.width = `${Math.max(2, Math.round(f * 100))}%`;
        lbl.textContent = t;
      },
      // Полную полосу заслуживает только успех. При отмене и при сбое полоса
      // замирает там, где остановилась (сбой ещё и краснеет): залитая до конца
      // выглядит как «установилось», и неудача становится неотличима от удачи.
      finish: (t: string, kind: "done" | "stopped" | "failed" = "done") => {
        if (kind === "done") fill.style.width = "100%";
        fill.classList.toggle("error", kind === "failed");
        lbl.textContent = t;
        lbl.classList.toggle("danger", kind === "failed");
      },
    };
  };

  // Итоги операций под строками прогресса: что именно не получилось и что с этим
  // делать. Вид сообщения (к сведению / ошибка) — общий для всех карточек.
  const notes: { text: string; kind: StatusKind }[] = [];
  if (source !== "internet") {
    // Импорт: копируется ВЕСЬ каталог носителя одной операцией (набор согласован
    // при сборке флешки), затем честно сверяем, что из выбранного появилось.
    const row = progressRow("Импорт моделей с носителя");
    const outcome = await runImport(source, {
      progress: row.progress,
      done: () => row.finish("Готово — носитель можно извлечь"),
      cancelled: () => row.finish("Импорт отменён — уже скопированное сохранено", "stopped"),
      error: (m) => row.finish(m, "failed"),
    });
    if (outcome === "error")
      notes.push({
        text:
          "Модели с носителя не скопировались. Проверьте, что флешка на месте и на диске " +
          "есть свободное место, затем повторите — либо установите модели из интернета " +
          "(Настройки → Модели).",
        kind: "error",
      });
    if (outcome === "done" && tags.length) {
      try {
        const st = await invoke<{ models: { tag: string; installed: boolean }[] }>(
          "model_states",
        );
        const missing = tags.filter(
          (t) => !st.models.find((m) => m.tag === t)?.installed,
        );
        if (missing.length)
          notes.push({
            text:
              `На носителе не оказалось: ${missing.join(", ")} — ` +
              "их можно установить из интернета (Настройки → Модели).",
            kind: "error",
          });
      } catch {
        // Движок молчит: копирование прошло, а вот появились ли нужные модели —
        // неизвестно. Обещать «всё на месте» в таком случае нельзя.
        notes.push({
          text:
            "Проверить, какие модели появились после импорта, не удалось: движок не " +
            "отвечает. Загляните позже в Настройки → Модели.",
          kind: "info",
        });
      }
    }
  } else {
    const failed: string[] = [];
    for (const tag of tags) {
      const a = assessments.find((x) => x.tag === tag);
      const row = progressRow(a ? `${a.title} (${tag})` : tag);
      const outcome = await runPull(tag, {
        progress: row.progress,
        done: () => row.finish("Установлена"),
        cancelled: () => row.finish("Отменена — докачка продолжится при повторе", "stopped"),
        error: (m) => row.finish(m, "failed"),
      });
      if (outcome === "error") failed.push(tag); // остальные всё же попробуем поставить
      if (outcome === "cancelled") {
        notes.push({
          text: "Установка остановлена — оставшиеся модели можно поставить позже (Настройки → Модели).",
          kind: "info",
        });
        break;
      }
    }
    if (failed.length)
      notes.push({
        text:
          `Не установлено: ${failed.join(", ")}. Обычно виноват пропавший интернет или ` +
          "нехватка места на диске — повторите позже (Настройки → Модели).",
        kind: "error",
      });
  }

  cancelBtn.hidden = true;
  await loadModels();
  await loadModelStates();
  await refreshDocuments(sidebarDocCtx);
  await refreshCurrentDocsCount();

  for (const n of notes) {
    const box = el("div");
    wizardCard.appendChild(box);
    showStatus(box, n.text, n.kind);
  }
  // Если что-то не установилось, главное действие — повторить: иначе экран с
  // красной строкой и единственной кнопкой «Начать работу» выглядит тупиком.
  const failedSomething = notes.some((n) => n.kind === "error");
  const actions = el("div", "wizard-actions");
  const doneBtn = el(
    "button",
    failedSomething ? "ep-btn ep-btn--ghost" : "ep-btn ep-btn--primary",
    "Начать работу",
  );
  doneBtn.type = "button";
  doneBtn.addEventListener("click", closeWizard);
  if (failedSomething) {
    const retryBtn = el("button", "ep-btn ep-btn--primary", "Попробовать снова");
    retryBtn.type = "button";
    retryBtn.addEventListener("click", () => void renderSetupScreen());
    actions.appendChild(retryBtn);
  }
  actions.appendChild(doneBtn);
  wizardCard.appendChild(actions);
}

export function wireWizard() {
  openWizardBtn.addEventListener("click", openWizard);
  wizardCloseBtn.addEventListener("click", closeWizard);
}
