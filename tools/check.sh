#!/usr/bin/env bash
# ── Полная проверка проекта на машине разработчика ────────────────────────────
#
# Делает то же, что делал CI: строгие типы, тесты, clippy без предупреждений,
# формат, аудит зависимостей и — главное — КОМПИЛЯЦИЮ ВЕТОК ДЛЯ LINUX, которых
# на macOS не существует (#[cfg(target_os = "linux")]).
#
# Зачем скрипт: пока GitHub Actions недоступны, единственная сетка безопасности —
# локальная. Её нельзя держать «в голове»: набор проверок должен быть один,
# воспроизводимый и одинаковый у любого, кто трогает код.
#
#   tools/check.sh              — быстрые проверки (без контейнера, ~1–2 минуты)
#   tools/check.sh --linux      — плюс сборка и тесты под Linux в Docker
#   tools/check.sh --all        — всё, включая аудит уязвимостей (нужна сеть)
#
# Выход 0 — всё чисто. Любая упавшая проверка обрывает скрипт с понятным сообщением.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

WITH_LINUX=0
WITH_AUDIT=0
for a in "$@"; do
  case "$a" in
    --linux) WITH_LINUX=1 ;;
    --all) WITH_LINUX=1; WITH_AUDIT=1 ;;
    -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
    *) echo "неизвестный флаг: $a (см. --help)" >&2; exit 2 ;;
  esac
done

FAILED=""
step() { printf '\n\033[1m── %s\033[0m\n' "$1"; }
ok() { printf '   ✓ %s\n' "$1"; }
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

step "Фронтенд: строгие типы, тесты, сборка"
try "типы (tsc --noEmit)" npx tsc --noEmit
try "тесты (vitest)" npm test --silent
try "сборка (vite build)" npm run build

step "Бэкенд на этой машине (macOS)"
try "clippy без предупреждений" \
  cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
try "тесты Rust" cargo test --manifest-path src-tauri/Cargo.toml
# Формат — информационно: кодовая база исторически не под rustfmt, валить на этом
# сборку значит приучить всех пропускать проверку целиком.
if cargo fmt --check --manifest-path src-tauri/Cargo.toml >/dev/null 2>&1; then
  ok "формат Rust"
else
  printf '   ~ формат Rust: есть расхождения (не блокирует)\n'
fi

step "Согласованность интерфейса"
# Классы, которые вешает JS, но которых нет в стилях, и наоборот — id из JS без
# элемента в разметке. Обе ошибки молча ломают интерфейс уже у пользователя.
try "id из JS существуют в index.html" python3 tools/check-dom.py

if [ "$WITH_LINUX" = 1 ]; then
  step "Сборка под Linux (Docker) — ветки cfg, невидимые на macOS"
  if ! docker info >/dev/null 2>&1; then
    printf '   ✗ Docker не запущен — проверка Linux пропущена\n'
    FAILED="$FAILED\n   • Linux-проверка (Docker не запущен)"
  else
    docker build -q -f tools/check-linux.dockerfile -t jai-check-linux . >/dev/null
    # Кэш cargo — томом: без него каждый прогон пересобирает все зависимости.
    try "clippy под Linux" docker run --rm \
      -v "$ROOT":/work -v jai-cargo-registry:/usr/local/cargo/registry \
      -v jai-linux-target:/work/src-tauri/target-linux \
      -e CARGO_TARGET_DIR=/work/src-tauri/target-linux \
      jai-check-linux \
      cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
    try "тесты Rust под Linux" docker run --rm \
      -v "$ROOT":/work -v jai-cargo-registry:/usr/local/cargo/registry \
      -v jai-linux-target:/work/src-tauri/target-linux \
      -e CARGO_TARGET_DIR=/work/src-tauri/target-linux \
      jai-check-linux \
      cargo test --manifest-path src-tauri/Cargo.toml
  fi
fi

if [ "$WITH_AUDIT" = 1 ]; then
  step "Уязвимости в зависимостях"
  try "npm audit (только то, что уезжает в приложение)" \
    npm audit --omit=dev --audit-level=high
  if command -v cargo-audit >/dev/null 2>&1; then
    try "cargo audit" cargo audit --file src-tauri/Cargo.lock
  else
    printf '   ~ cargo-audit не установлен: cargo install cargo-audit --locked\n'
  fi
fi

echo
if [ -n "$FAILED" ]; then
  printf '\033[1mЕсть упавшие проверки:\033[0m%b\n\n' "$FAILED"
  exit 1
fi
printf '\033[1mВсё чисто.\033[0m\n'
[ "$WITH_LINUX" = 1 ] || printf 'Ветки для Linux не проверялись — запустите с --linux.\n'
echo
