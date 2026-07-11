// Онлайн-режим (агентный, tool calling): тумблеры, настройки веб-поиска и журнал
// исходящих обращений (прозрачность, 152-ФЗ). По умолчанию ВЫКЛЮЧЕН.

import { invoke } from "@tauri-apps/api/core";
import type { OutboundLogEntry } from "./types";
import { state } from "./state";
import {
  onlineBadgeEl,
  onlineMasterToggleEl,
  onlineStateTextEl,
  onlineStatusEl,
  onlineToggleEl,
  outboundClearBtn,
  outboundLogEl,
  outboundRefreshBtn,
  wsKeyEl,
  wsProviderEl,
  wsUrlEl,
} from "./dom";

// Применяет состояние онлайн-режима ко всему UI: тумблер в композере, индикатор в
// шапке (виден только в онлайне — в офлайне обычная работа), подпись и кнопка в
// настройках. `persist` — записать выбор в settings.json (по умолчанию да).
function setOnlineMode(on: boolean, persist = true) {
  state.onlineMode = on;
  onlineToggleEl.classList.toggle("on", on);
  onlineBadgeEl.hidden = !on; // индикатор «данные могут уходить наружу» только в онлайне
  onlineStateTextEl.textContent = on ? "включён" : "выключен";
  onlineMasterToggleEl.textContent = on ? "Выключить" : "Включить";
  if (persist) {
    invoke("set_setting", { key: "online_mode", value: on ? "1" : "0" }).catch((e) =>
      console.error("set_setting online_mode:", e),
    );
  }
}

// Восстанавливает онлайн-режим и настройки веб-поиска из settings.json и навешивает
// обработчики. Онлайн ВЫКЛЮЧЕН, пока пользователь явно не включал (152-ФЗ).
export async function initOnline() {
  // Состояние режима (по умолчанию выкл).
  let onlineSaved: string | null = null;
  try {
    onlineSaved = await invoke<string | null>("get_setting", { key: "online_mode" });
  } catch {
    onlineSaved = null;
  }
  setOnlineMode(onlineSaved === "1", false); // только "1" = включён; иначе офлайн

  // Поля провайдера/эндпоинта/ключа (дефолты — Tavily; пустой ключ = поиск недоступен).
  const load = async (key: string) => {
    try {
      return (await invoke<string | null>("get_setting", { key })) ?? "";
    } catch {
      return "";
    }
  };
  wsProviderEl.value = (await load("web_search_provider")) || "tavily";
  wsUrlEl.value = (await load("web_search_url")) || "https://api.tavily.com/search";
  wsKeyEl.value = await load("web_search_api_key");

  // Сохраняем поля по уходу фокуса (как другие настройки — единый источник settings.json).
  const saveField = (el: HTMLInputElement, key: string) => {
    el.addEventListener("change", () => {
      invoke("set_setting", { key, value: el.value.trim() }).catch((e) =>
        console.error(`set_setting ${key}:`, e),
      );
    });
  };
  saveField(wsProviderEl, "web_search_provider");
  saveField(wsUrlEl, "web_search_url");
  saveField(wsKeyEl, "web_search_api_key");

  // Тумблеры (композер + мастер в настройках) переключают один и тот же режим.
  onlineToggleEl.addEventListener("click", () => setOnlineMode(!state.onlineMode));
  onlineMasterToggleEl.addEventListener("click", () => setOnlineMode(!state.onlineMode));

  // Журнал исходящих обращений.
  outboundRefreshBtn.addEventListener("click", refreshOutboundLog);
  outboundClearBtn.addEventListener("click", async () => {
    try {
      await invoke("clear_outbound_log");
      await refreshOutboundLog();
    } catch (e) {
      showOnlineStatus(`Не удалось очистить журнал: ${e}`);
    }
  });
}

// Подтягивает журнал обращений в интернет и рисует его (новые сверху). Прозрачность
// постфактум: что и на какой хост ушло (152-ФЗ при отсутствии пер-запросного запроса).
export async function refreshOutboundLog() {
  let entries: OutboundLogEntry[] = [];
  try {
    entries = await invoke<OutboundLogEntry[]>("get_outbound_log");
  } catch (e) {
    outboundLogEl.textContent = `Не удалось загрузить журнал: ${e}`;
    return;
  }
  outboundLogEl.innerHTML = "";
  if (!entries.length) {
    const empty = document.createElement("div");
    empty.className = "online-log__empty";
    empty.textContent = "Обращений в интернет пока не было.";
    outboundLogEl.appendChild(empty);
    return;
  }
  for (const e of entries) {
    const row = document.createElement("div");
    row.className = "online-log__row";
    const when = new Date(e.ts).toLocaleString();
    const meta = document.createElement("div");
    meta.className = "online-log__meta";
    meta.textContent = `${when} · ${e.host} · ${e.tool}`;
    const q = document.createElement("div");
    q.className = "online-log__query";
    q.textContent = e.query ? `«${e.query}»` : "";
    row.append(meta, q);
    outboundLogEl.appendChild(row);
  }
}

function showOnlineStatus(text: string) {
  onlineStatusEl.textContent = text;
  onlineStatusEl.hidden = false;
}
