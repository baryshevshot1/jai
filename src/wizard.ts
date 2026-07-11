// Мастер установки: открывается сам при первом запуске (обязательные модели не
// установлены) и по кнопке из настроек. Оценивает машину, предлагает ПОСИЛЬНЫЕ
// модели набора (честные вердикты «потянет/впритык/не потянет» — та же формула
// памяти, что защищает от свопа) и ставит выбранное с флешки (импорт) или из
// интернета. Вся логика — готовые команды provision/pull; мастер только интерфейс.

import { invoke } from "@tauri-apps/api/core";
import type { HardwareInfo, ModelAssessment, ModelSource } from "./types";
import {
  composerWrapEl,
  feedEl,
  openWizardBtn,
  projectView,
  settingsBtn,
  settingsView,
  wizardCard,
  wizardCloseBtn,
  wizardView,
} from "./dom";
import { plural } from "./util";
import { showChatView } from "./ui";
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
  feedEl.hidden = true;
  composerWrapEl.hidden = true;
  settingsView.hidden = true;
  settingsBtn.classList.remove("active");
  projectView.hidden = true;
  wizardView.hidden = false;
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
  const installBtn = el("button", "ep-btn", "Установить выбранные");
  installBtn.type = "button";
  const laterBtn = el("button", "ep-btn ep-btn--ghost", "Позже");
  laterBtn.type = "button";
  actions.append(installBtn, laterBtn);
  wizardCard.appendChild(actions);
  const status = el("div", "wizard-note", "");
  wizardCard.appendChild(status);

  laterBtn.addEventListener("click", closeWizard);

  // Железо и движок — параллельно, не задерживая список моделей.
  void (async () => {
    const parts: string[] = [];
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
      parts.push("не удалось определить железо");
    }
    try {
      const v = await invoke<string>("ollama_version");
      parts.push(`движок Ollama ${v}`);
    } catch {
      parts.push("движок недоступен: установка из интернета не сработает, импорт с флешки — доступен");
    }
    hwLine.textContent = parts.join(" · ");
  })();

  // Оценка набора против железа (работает и без движка — по приблизительным весам).
  let assessments: ModelAssessment[] = [];
  try {
    assessments = await invoke<ModelAssessment[]>("assess_models");
  } catch (e) {
    listEl.appendChild(el("div", "wizard-note", `Не удалось оценить набор моделей: ${e}`));
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
    try {
      sources = await invoke<ModelSource[]>("find_model_sources");
    } catch {
      /* без носителей остаётся интернет */
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
  };
  await renderSources();
  srcRefresh.addEventListener("click", () => void renderSources());

  installBtn.addEventListener("click", async () => {
    const chosen = assessments.filter((a) => selected.has(a.tag)).map((a) => a.tag);
    const src =
      srcList.querySelector<HTMLInputElement>('input[name="wizard-src"]:checked')?.value ??
      "internet";
    if (!chosen.length && src === "internet") {
      status.textContent = "Отметьте хотя бы одну модель.";
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
      finish: (t: string, err = false) => {
        fill.style.width = "100%";
        lbl.textContent = t;
        lbl.classList.toggle("danger", err);
      },
    };
  };

  const notes: string[] = [];
  if (source !== "internet") {
    // Импорт: копируется ВЕСЬ каталог носителя одной операцией (набор согласован
    // при сборке флешки), затем честно сверяем, что из выбранного появилось.
    const row = progressRow("Импорт моделей с носителя");
    const outcome = await runImport(source, {
      progress: row.progress,
      done: () => row.finish("Готово — носитель можно извлечь"),
      cancelled: () => row.finish("Импорт отменён — уже скопированное сохранено"),
      error: (m) => row.finish(m, true),
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
          notes.push(
            `На носителе не оказалось: ${missing.join(", ")} — их можно установить из интернета (Настройки → Модели).`,
          );
      } catch {
        /* движок молчит — состояние покажет список моделей позже */
      }
    }
  } else {
    for (const tag of tags) {
      const a = assessments.find((x) => x.tag === tag);
      const row = progressRow(a ? `${a.title} (${tag})` : tag);
      const outcome = await runPull(tag, {
        progress: row.progress,
        done: () => row.finish("Установлена"),
        cancelled: () => row.finish("Отменена — докачка продолжится при повторе"),
        error: (m) => row.finish(m, true),
      });
      if (outcome === "cancelled") {
        notes.push(
          "Установка остановлена — оставшиеся модели можно поставить позже (Настройки → Модели).",
        );
        break;
      }
    }
  }

  cancelBtn.hidden = true;
  await loadModels();
  await loadModelStates();
  await refreshDocuments(sidebarDocCtx);
  await refreshCurrentDocsCount();

  for (const n of notes) wizardCard.appendChild(el("div", "wizard-note", n));
  const actions = el("div", "wizard-actions");
  const doneBtn = el("button", "ep-btn", "Начать работу");
  doneBtn.type = "button";
  doneBtn.addEventListener("click", closeWizard);
  actions.appendChild(doneBtn);
  wizardCard.appendChild(actions);
}

export function wireWizard() {
  openWizardBtn.addEventListener("click", openWizard);
  wizardCloseBtn.addEventListener("click", closeWizard);
}
