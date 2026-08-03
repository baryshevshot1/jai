#!/usr/bin/env bash
# ── Полная проверка проекта на машине разработчика ────────────────────────────
#
# Единственная сетка безопасности этого проекта. GitHub Actions недоступны (аккаунт
# заблокирован по оплате), поэтому красный и зелёный свет мы даём себе сами — и
# гейт от этого должен быть СТРОЖЕ, а не мягче. Держать набор проверок «в голове»
# нельзя: он должен быть один, воспроизводимый и одинаковый у всех, кто трогает код.
#
# ЯРУСЫ. Гейт, который на каждый коммит гоняет всё, перестают запускать; гейт,
# который не гоняет ничего, ничего и не ловит. Отсюда три уровня:
#
#   tools/check.sh --fast       формат, типы, быстрые тесты          — секунды
#   tools/check.sh              + clippy, сборка, DOM, контракт      — 1–2 минуты
#   tools/check.sh --full       + матрица фич, Linux в Docker,       — минуты
#                                 кросс-проверка Windows
#   tools/check.sh --release    + аудит зависимостей и smoke-тест    — десятки минут
#                                 собранного бандла (--self-check)
#
# Отдельные флаги (складываются): --linux, --features, --windows, --smoke, --audit.
# Старое поведение сохранено: --all == --linux --audit.
#
# ЗЕЛЁНЫЙ ШТАМП. Прогон уровня --full и выше при успехе пишет .check/last-green.json:
# коммит, ветка, ярус, набор проверок, версии инструментов. tools/release.sh
# отказывается собирать бандл, если штамп не совпадает с текущим состоянием дерева.
# Это заменяет branch protection, которого без CI взять негде.
#
# Выход 0 — всё чисто. Любая упавшая проверка перечисляется в конце.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

TIER="normal"
WITH_LINUX=0
WITH_AUDIT=0
WITH_FEATURES=0
WITH_WINDOWS=0
WITH_SMOKE=0
FAST_ONLY=0

for a in "$@"; do
  case "$a" in
    --fast) FAST_ONLY=1; TIER="fast" ;;
    --linux) WITH_LINUX=1 ;;
    --features) WITH_FEATURES=1 ;;
    --windows) WITH_WINDOWS=1 ;;
    --smoke) WITH_SMOKE=1 ;;
    --audit) WITH_AUDIT=1 ;;
    --full) WITH_LINUX=1; WITH_FEATURES=1; WITH_WINDOWS=1; TIER="full" ;;
    --release)
      WITH_LINUX=1; WITH_FEATURES=1; WITH_WINDOWS=1; WITH_SMOKE=1; WITH_AUDIT=1
      TIER="release" ;;
    --all) WITH_LINUX=1; WITH_AUDIT=1 ;;
    -h|--help) sed -n '2,28p' "$0"; exit 0 ;;
    *) echo "неизвестный флаг: $a (см. --help)" >&2; exit 2 ;;
  esac
done

FAILED=""
SKIPPED=""
RAN=""
step() { printf '\n\033[1m── %s\033[0m\n' "$1"; }
ok() { printf '   ✓ %s\n' "$1"; RAN="$RAN$1;"; }
# Пропуск ОБЪЯВЛЯЕМ, а не замалчиваем: молча выпавшая проверка читается как
# «проверено», и именно так дыры в приёмке становятся невидимыми.
skip() { printf '   ~ %s — ПРОПУЩЕНО: %s\n' "$1" "$2"; SKIPPED="$SKIPPED\n   • $1 ($2)"; }
# Проверку не обрываем на первой ошибке: полезнее увидеть ВСЕ упавшие сразу,
# чем чинить их по одной, каждый раз прогоняя всё заново.
try() { # try <название> <команда...>
  local name="$1"; shift
  if "$@" >/tmp/jai-check.log 2>&1; then
    ok "$name"
  else
    printf '   ✗ %s\n' "$name"
    tail -25 /tmp/jai-check.log | sed 's/^/     /'
    FAILED="$FAILED\n   • $name"
  fi
}

step "Фронтенд: строгие типы и тесты"
try "типы (tsc --noEmit)" npx tsc --noEmit
try "тесты (vitest)" npm test --silent

if [ "$FAST_ONLY" = 1 ]; then
  step "Быстрый ярус: сборка и clippy пропущены (см. без --fast)"
  # Формат Rust — информационно и здесь: он дёшев, а падать на нём значит приучить
  # пропускать хук целиком через --no-verify.
  if cargo fmt --check --manifest-path src-tauri/Cargo.toml >/dev/null 2>&1; then
    ok "формат Rust"
  else
    printf '   ~ формат Rust: есть расхождения (не блокирует)\n'
  fi
  try "id из JS существуют в index.html" python3 tools/check-dom.py
else
  try "сборка (vite build)" npm run build

  step "Бэкенд на этой машине (macOS)"
  try "clippy без предупреждений" \
    cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
  try "тесты Rust" cargo test --manifest-path src-tauri/Cargo.toml
  if cargo fmt --check --manifest-path src-tauri/Cargo.toml >/dev/null 2>&1; then
    ok "формат Rust"
  else
    printf '   ~ формат Rust: есть расхождения (не блокирует)\n'
  fi

  step "Согласованность интерфейса"
  # Классы, которые вешает JS, но которых нет в стилях, и наоборот — id из JS без
  # элемента в разметке. Обе ошибки молча ломают интерфейс уже у пользователя.
  # Здесь же — контракт команд: каждый invoke обязан быть зарегистрирован в Rust.
  try "id из JS существуют в index.html" python3 tools/check-dom.py
fi

if [ "$WITH_FEATURES" = 1 ]; then
  step "Матрица фич — сборка без голоса и со всем сразу"
  # Голос — cargo-фича, включённая по умолчанию. Сборка БЕЗ неё обязана оставаться
  # рабочей: whisper тянет cmake и C++-тулчейн, и на машине без них приложение
  # должно собираться и честно говорить «голосовой ввод не собран», а не падать.
  try "clippy --no-default-features" \
    cargo clippy --manifest-path src-tauri/Cargo.toml --no-default-features --all-targets -- -D warnings
  try "тесты --no-default-features" \
    cargo test --manifest-path src-tauri/Cargo.toml --no-default-features
  try "clippy --all-features" \
    cargo clippy --manifest-path src-tauri/Cargo.toml --all-features --all-targets -- -D warnings
fi

if [ "$WITH_LINUX" = 1 ]; then
  step "Сборка под Linux (Docker) — ветки cfg, невидимые на macOS"
  if ! docker info >/dev/null 2>&1; then
    printf '   ✗ Docker не запущен — проверка Linux пропущена\n'
    FAILED="$FAILED\n   • Linux-проверка (Docker не запущен)"
  else
    docker build -q -f tools/check-linux.dockerfile -t jai-check-linux . >/dev/null
    # Кэш cargo — томом: без него каждый прогон пересобирает все зависимости.
    # Голосовой ввод из проверки под Linux исключён: whisper.cpp не собирается на
    # ARM64 (контейнер на Apple Silicon) — системный GCC не включает fp16-инструкции.
    # Целевые платформы продукта — x86_64 Linux и Windows, там сборка штатная; здесь
    # же важнее проверить ВЕСЬ остальной Linux-код, чем потерять проверку целиком.
    # Сам модуль голоса проверяется на macOS, где он собирается полностью.
    VOICE_OFF="--no-default-features"
    try "clippy под Linux (без голосового ввода)" docker run --rm \
      -v "$ROOT":/work -v jai-cargo-registry:/usr/local/cargo/registry \
      -v jai-linux-target:/work/src-tauri/target-linux \
      -e CARGO_TARGET_DIR=/work/src-tauri/target-linux \
      jai-check-linux \
      cargo clippy --manifest-path src-tauri/Cargo.toml $VOICE_OFF --all-targets -- -D warnings
    try "тесты Rust под Linux (без голосового ввода)" docker run --rm \
      -v "$ROOT":/work -v jai-cargo-registry:/usr/local/cargo/registry \
      -v jai-linux-target:/work/src-tauri/target-linux \
      -e CARGO_TARGET_DIR=/work/src-tauri/target-linux \
      jai-check-linux \
      cargo test --manifest-path src-tauri/Cargo.toml $VOICE_OFF
  fi
fi

if [ "$WITH_WINDOWS" = 1 ]; then
  step "Кросс-проверка Windows — ветки #[cfg(windows)], которых здесь не существует"
  # Windows не проверялся НИЧЕМ: ни сборкой, ни запуском. Полностью дыру это не
  # закрывает (нужен стенд — см. docs/RELEASE.md), но ловит целый класс отказов:
  # непортируемые пути, разделители, ветки под cfg(windows), которые на Mac просто
  # не компилируются и потому никогда не проверялись.
  #
  # Только --no-default-features: whisper-rs собирает C++ через cmake, и кросс-сборка
  # его тулчейном xwin не проходит. Это ЗАПИСАННОЕ ограничение, а не тихо выкинутый
  # шаг: модуль голоса под Windows не проверен ничем.
  # Для кросс-сборки под MSVC нужны три инструмента LLVM: llvm-lib (замена lib.exe),
  # lld-link (компоновщик) и llvm-rc (компилятор ресурсов Windows — иконка и версия
  # приложения). Первые два берём из rustup (`llvm-tools`): llvm-lib — это тот же
  # llvm-ar, он смотрит на имя, под которым его позвали, поэтому хватает симлинка.
  # llvm-rc в rustup НЕТ — он есть только в полном LLVM (brew install llvm).
  SHIM="$ROOT/.check/win-tools"
  win_ready=1
  win_why=""
  if ! command -v cargo-xwin >/dev/null 2>&1; then
    win_ready=0; win_why="нет cargo-xwin (cargo install cargo-xwin --locked)"
  elif ! rustup target list --installed 2>/dev/null | grep -q x86_64-pc-windows-msvc; then
    win_ready=0; win_why="нет цели (rustup target add x86_64-pc-windows-msvc)"
  else
    RUSTBIN="$(rustc --print sysroot)/lib/rustlib/$(rustc -vV | awk '/^host:/{print $2}')/bin"
    mkdir -p "$SHIM"
    [ -x "$RUSTBIN/llvm-ar" ] && ln -sf "$RUSTBIN/llvm-ar" "$SHIM/llvm-lib"
    [ -x "$RUSTBIN/gcc-ld/lld-link" ] && ln -sf "$RUSTBIN/gcc-ld/lld-link" "$SHIM/lld-link"
    # Полный LLVM, если он есть, — приоритетнее: там инструменты полного комплекта.
    for d in /opt/homebrew/opt/llvm/bin /usr/local/opt/llvm/bin; do
      [ -d "$d" ] && export PATH="$d:$PATH"
    done
    export PATH="$SHIM:$PATH"
    for t in llvm-lib lld-link llvm-rc; do
      command -v "$t" >/dev/null 2>&1 || { win_ready=0; win_why="нет $t (brew install llvm)"; }
    done
    if [ "$win_ready" = 0 ] && ! rustup component list --installed 2>/dev/null | grep -q llvm-tools; then
      win_why="$win_why; также: rustup component add llvm-tools"
    fi
  fi

  if [ "$win_ready" = 0 ]; then
    skip "cargo check под Windows" "$win_why"
  else
    try "cargo check под Windows (без голосового ввода)" \
      cargo xwin check --manifest-path src-tauri/Cargo.toml \
      --target x86_64-pc-windows-msvc --no-default-features
  fi
fi

if [ "$WITH_AUDIT" = 1 ]; then
  step "Уязвимости в зависимостях"
  try "npm audit (только то, что уезжает в приложение)" \
    npm audit --omit=dev --audit-level=high
  if command -v cargo-audit >/dev/null 2>&1; then
    try "cargo audit" cargo audit --file src-tauri/Cargo.lock
  else
    skip "cargo audit" "не установлен (cargo install cargo-audit --locked)"
  fi
fi

if [ "$WITH_SMOKE" = 1 ]; then
  step "Smoke-тест собранного бандла"
  # Единственная проверка, которая имеет дело с ТЕМ САМЫМ бинарником, а не с
  # исходниками. Отвечает на вопрос, который иначе решается памятью: собралось ли,
  # зарегистрированы ли команды, доступны ли каталоги данных. Окно не открывается —
  # --self-check печатает JSON и выходит.
  if bash tools/smoke.sh; then
    ok "бандл собран и отвечает на --self-check"
  else
    printf '   ✗ smoke-тест бандла\n'
    FAILED="$FAILED\n   • smoke-тест бандла"
  fi
fi

# ── Зелёный штамп ────────────────────────────────────────────────────────────
# Пишется только при ПОЛНОМ успехе и только для ярусов full/release: штамп — это
# утверждение «вот это дерево проверено вот настолько», и слабый прогон не имеет
# права его выдавать. Пропущенные шаги штамп тоже запрещают: «проверено кроме
# Windows» и «проверено» — разные вещи, и различать их должен файл, а не память.
write_stamp() {
  mkdir -p "$ROOT/.check"
  cat > "$ROOT/.check/last-green.json" <<JSON
{
  "sha": "$(git rev-parse HEAD 2>/dev/null || echo неизвестно)",
  "branch": "$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo неизвестно)",
  "clean_tree": $( [ -z "$(git status --porcelain 2>/dev/null)" ] && echo true || echo false ),
  "tier": "$TIER",
  "at": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "host_os": "$(uname -s) $(uname -r) $(uname -m)",
  "rustc": "$(rustc --version 2>/dev/null || echo неизвестно)",
  "cargo": "$(cargo --version 2>/dev/null || echo неизвестно)",
  "node": "$(node --version 2>/dev/null || echo неизвестно)",
  "npm": "$(npm --version 2>/dev/null || echo неизвестно)",
  "checks": "$(printf '%s' "$RAN")"
}
JSON
  printf 'Зелёный штамп записан: .check/last-green.json (ярус %s)\n' "$TIER"
}

echo
if [ -n "$FAILED" ]; then
  printf '\033[1mЕсть упавшие проверки:\033[0m%b\n' "$FAILED"
  [ -n "$SKIPPED" ] && printf '\033[1mПропущено:\033[0m%b\n' "$SKIPPED"
  # Штамп при неудаче убираем: устаревший «зелёный» опаснее его отсутствия.
  rm -f "$ROOT/.check/last-green.json"
  echo
  exit 1
fi

printf '\033[1mВсё чисто.\033[0m\n'
if [ -n "$SKIPPED" ]; then
  printf '\033[1mНо часть проверок пропущена:\033[0m%b\n' "$SKIPPED"
  printf 'Штамп не выдан — это неполный прогон.\n'
  rm -f "$ROOT/.check/last-green.json"
elif [ "$TIER" = "full" ] || [ "$TIER" = "release" ]; then
  write_stamp
else
  [ "$WITH_LINUX" = 1 ] || printf 'Ветки для Linux не проверялись — запустите с --linux (или --full).\n'
fi
echo
