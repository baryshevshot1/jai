#!/usr/bin/env python3
"""Сверка разметки и кода: id, которые ищет JS, обязаны существовать в index.html.

Реестр DOM (src/dom.ts) разбирает элементы по id с non-null assertion: опечатка в
имени даёт null, и приложение падает при старте — у пользователя это «белый экран».
Компилятор такое не ловит, тесты тоже (реестр требует живого документа), поэтому
проверка вынесена отдельно и входит в tools/check.sh.

Элементы, которые JS создаёт сам, а потом ищет в СОБСТВЕННОМ поддереве, — не ошибка:
их в index.html нет по определению. Такие случаи перечислены в DYNAMIC.
"""
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# id, которых нет и не должно быть в index.html: элементы создаются кодом и ищутся
# в СОБСТВЕННОМ поддереве (card.querySelector), а не в документе — уронить старт они
# не могут. Карточки «Оформление» и «Голосовой ввод» в настройках строятся в
# settings.ts. Добавляя сюда id, убедись, что поиск идёт именно по своему узлу.
DYNAMIC = {
    "ui-scale-state",
    "ui-scale-toggle",
    "voice-model-state",
    "voice-model-install",
    "voice-model-restart",
    "voice-model-import",
    "voice-model-progress",
    "voice-model-progress-fill",
    "voice-model-progress-label",
    "voice-model-cancel",
}


def check_ipc_contract() -> list[str]:
    """Каждая команда, которую зовёт интерфейс, должна быть зарегистрирована в Rust.

    Иначе Tauri отвечает отказом уже в работающем приложении: кнопка молча ничего не
    делает, а причина видна только в консоли разработчика, куда пользователь не ходит.
    """
    lib = (ROOT / "src-tauri" / "src" / "lib.rs").read_text(encoding="utf-8")
    m = re.search(r"generate_handler!\[(.*?)\]", lib, re.S)
    if not m:
        return ["не найден список команд generate_handler! в lib.rs"]
    registered = {x.strip().split("::")[-1] for x in m.group(1).split(",") if x.strip()}

    problems = []
    for path in sorted((ROOT / "src").glob("*.ts")):
        if path.name.endswith(".test.ts"):
            continue
        text = path.read_text(encoding="utf-8")
        for mm in re.finditer(r'invoke(?:<[^>]*>)?\(\s*"([a-z_]+)"', text):
            if mm.group(1) not in registered:
                problems.append(f"{path.name}: invoke(\"{mm.group(1)}\") — команда не зарегистрирована")
    return sorted(set(problems))


def main() -> int:
    html = (ROOT / "index.html").read_text(encoding="utf-8")
    html_ids = set(re.findall(r'\bid="([^"]+)"', html))

    missing: list[str] = []
    for path in sorted((ROOT / "src").glob("*.ts")):
        if path.name.endswith(".test.ts"):
            continue
        text = path.read_text(encoding="utf-8")
        found = re.findall(r'getElementById\(\s*"([^"]+)"\s*\)', text)
        found += re.findall(r'querySelector(?:All)?\(\s*"#([\w-]+)"', text)
        for name in found:
            if name not in html_ids and name not in DYNAMIC:
                missing.append(f"{path.name}: #{name}")

    if missing:
        print("id, которые ищет код, но которых нет в index.html:")
        for m in sorted(set(missing)):
            print(f"  {m}")
        print("\nЭто уронит приложение при старте. Проверьте опечатку либо, если")
        print("элемент создаётся кодом, добавьте его id в DYNAMIC в этом файле.")
        return 1

    ipc = check_ipc_contract()
    if ipc:
        print("Команды, которые зовёт интерфейс, но которых нет в бэкенде:")
        for p in ipc:
            print(f"  {p}")
        return 1

    print(f"проверено id: {len(html_ids)} в разметке, все ссылки из кода разрешаются")
    print("контракт команд: все вызовы invoke разрешаются в lib.rs")
    return 0


if __name__ == "__main__":
    sys.exit(main())
