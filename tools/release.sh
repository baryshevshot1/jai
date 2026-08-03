#!/usr/bin/env bash
# ── Сборка бандла для выпуска ─────────────────────────────────────────────────
#
# Заменяет branch protection, которого без CI взять негде. Смысл один: НЕЛЬЗЯ
# собрать бандл для клиента из дерева, которое не прошло полный гейт. Проверяется
# не намерение, а факт — зелёный штамп .check/last-green.json, записанный самим
# гейтом, должен указывать на ТОТ ЖЕ коммит и чистое дерево.
#
#   tools/release.sh              собрать бандл (требует зелёного штампа)
#   tools/release.sh --check      только проверить готовность, ничего не собирая
#
# Рядом с бандлом кладётся build-report.json: что за коммит, каким ярусом проверено,
# какими версиями инструментов собрано. Для коробочного продукта это ещё и документ
# поставки — «что именно установлено у клиента» иначе не восстановить.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

CHECK_ONLY=0
for a in "$@"; do
  case "$a" in
    --check) CHECK_ONLY=1 ;;
    -h|--help) sed -n '2,17p' "$0"; exit 0 ;;
    *) echo "неизвестный флаг: $a" >&2; exit 2 ;;
  esac
done

STAMP="$ROOT/.check/last-green.json"
die() { printf '\033[1mВыпуск остановлен:\033[0m %s\n' "$1" >&2; exit 1; }

# Значение из штампа. Без jq: гейт обязан работать на любой машине разработчика,
# и тащить ради трёх строк ещё одну зависимость незачем.
stamp_value() {
  python3 -c "import json,sys;print(json.load(open(sys.argv[1])).get(sys.argv[2],''))" \
    "$STAMP" "$1" 2>/dev/null || true
}

echo "── проверка готовности к выпуску ──"

[ -f "$STAMP" ] || die "нет зелёного штампа. Прогоните: tools/check.sh --release"

HEAD_SHA="$(git rev-parse HEAD)"
STAMP_SHA="$(stamp_value sha)"
STAMP_TIER="$(stamp_value tier)"
STAMP_CLEAN="$(stamp_value clean_tree)"

[ "$STAMP_SHA" = "$HEAD_SHA" ] || die \
  "штамп относится к другому коммиту.
   проверено: $STAMP_SHA
   собираем:  $HEAD_SHA
   Прогоните: tools/check.sh --release"

# Только --release, НЕ --full. Разница решающая: аудит зависимостей входит лишь в
# релизный ярус, а автоматических проверок больше нет вовсе — ci.yml и audit.yml
# переведены на ручной запуск. Принимай мы здесь штамп «full», единственная в проекте
# проверка на уязвимости не выполнялась бы перед выпуском НИКОГДА, и бандл с дырявой
# зависимостью спокойно уезжал бы клиенту, который потом годами не обновляется.
case "$STAMP_TIER" in
  release) ;;
  # Скобки вокруг имени обязательны: следом идёт «»», и без них bash прихватывает
  # его первый байт в имя переменной — с set -u это падает «unbound variable».
  *) die "штамп выдан ярусом «${STAMP_TIER}», а выпуск требует --release
   (только он прогоняет аудит зависимостей и smoke-тест собранного бандла).
   Прогоните: tools/check.sh --release" ;;
esac

# Грязное дерево значит, что бинарник не соответствует ни одному коммиту:
# воспроизвести такую сборку потом будет нечем, а у клиента она останется годами.
DIRTY="$(git status --porcelain)"
[ -z "$DIRTY" ] || die "в рабочем дереве есть несохранённые изменения:
$(printf '%s' "$DIRTY" | sed 's/^/   /')
   Зафиксируйте или отложите их и прогоните гейт заново."

[ "$STAMP_CLEAN" = "True" ] || [ "$STAMP_CLEAN" = "true" ] || die \
  "гейт прогонялся на грязном дереве — его результат ничего не гарантирует."

printf '   ✓ штамп совпадает с HEAD (%s), ярус %s, дерево чистое\n' \
  "${HEAD_SHA:0:12}" "$STAMP_TIER"

if [ "$CHECK_ONLY" = 1 ]; then
  echo "── готово к выпуску (сборка не запускалась) ──"
  exit 0
fi

echo "── сборка ──"
export PATH="/opt/homebrew/bin:$PATH"
npx tauri build

# ── Документ поставки ────────────────────────────────────────────────────────
OUT="$ROOT/src-tauri/target/release/bundle"
mkdir -p "$OUT"
cat > "$OUT/build-report.json" <<JSON
{
  "app_version": "$(python3 -c "import json;print(json.load(open('src-tauri/tauri.conf.json'))['version'])")",
  "sha": "$HEAD_SHA",
  "branch": "$(git rev-parse --abbrev-ref HEAD)",
  "built_at": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "built_on": "$(uname -s) $(uname -r) $(uname -m)",
  "gate_tier": "$STAMP_TIER",
  "gate_at": "$(stamp_value at)",
  "rustc": "$(rustc --version)",
  "node": "$(node --version)",
  "known_gaps": [
    "Windows: собранный установщик не запускался ни разу — стенда нет (docs/RELEASE.md)",
    "Голосовой ввод под Windows не проверен даже кросс-сборкой (whisper тянет cmake)"
  ]
}
JSON
printf '\n\033[1mГотово.\033[0m Отчёт о сборке: %s\n' "$OUT/build-report.json"
