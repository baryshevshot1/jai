#!/usr/bin/env bash
# ── Сборка флешки «под ключ» для Jarvis AI ────────────────────────────────────
#
# Собирает на указанном носителе структуру для установки на чистой машине:
#
#   <флешка>/
#     ПРОЧТИ-МЕНЯ.html            — инструкция для того, кто устанавливает
#     Windows/  Установить.bat + Jarvis.AI_…-setup.exe + OllamaSetup.exe
#     Linux/    install.sh + .deb + .AppImage + ollama-linux-amd64.tgz
#     macOS/    Jarvis.AI_….dmg
#     models/   каталог моделей Ollama (manifests/ + blobs/) — приложение
#               найдёт его само и предложит импорт (мастер установки)
#
# Части НЕЗАВИСИМЫ — можно заказать только инсталляторы или только модели:
#   tools/make-usb.sh --out /Volumes/JAI --installers
#   tools/make-usb.sh --out /Volumes/JAI --models "qwen3.5:9b bge-m3"
#   tools/make-usb.sh --out /Volumes/JAI --installers --models "qwen3.5:9b qwen3.5:4b bge-m3"
#
# Флаги:
#   --out DIR         куда собирать (корень флешки); ОБЯЗАТЕЛЬНЫЙ
#   --installers      скачать установщики приложения (последний релиз GitHub
#                     или --version app-vX.Y.Z) и движка Ollama + скрипты usb/
#   --models "TAGS"   вытянуть модели (ollama pull) и выборочно скопировать
#                     их blobs/manifests в models/ (нужны ollama и jq)
#   --version TAG     конкретный тег релиза (по умолчанию — последний)
#
# ВАЖНО: флешку форматировать в exFAT — блоб модели 9B это ОДИН файл ~6 ГБ,
# FAT32 такие не хранит (лимит 4 ГБ). Объём: базовый набор — от 16 ГБ.
set -euo pipefail

REPO="baryshevshot1/jai"
OLLAMA_WIN_URL="https://ollama.com/download/OllamaSetup.exe"
OLLAMA_LINUX_URL="https://ollama.com/download/ollama-linux-amd64.tgz"

OUT=""
DO_INSTALLERS=0
MODELS=""
VERSION=""

die() { echo "ошибка: $*" >&2; exit 1; }
note() { echo "── $*"; }

while [ $# -gt 0 ]; do
  case "$1" in
    --out) OUT="${2:-}"; shift 2 ;;
    --installers) DO_INSTALLERS=1; shift ;;
    --models) MODELS="${2:-}"; shift 2 ;;
    --version) VERSION="${2:-}"; shift 2 ;;
    -h|--help) sed -n '2,30p' "$0"; exit 0 ;;
    *) die "неизвестный флаг: $1 (см. --help)" ;;
  esac
done

[ -n "$OUT" ] || die "укажите --out <каталог флешки>"
[ "$DO_INSTALLERS" = 1 ] || [ -n "$MODELS" ] || die "нечего делать: добавьте --installers и/или --models"
mkdir -p "$OUT"

SELF_DIR="$(cd "$(dirname "$0")" && pwd)"
USB_TPL="$SELF_DIR/../usb"

# ── Установщики ──────────────────────────────────────────────────────────────
if [ "$DO_INSTALLERS" = 1 ]; then
  command -v curl >/dev/null || die "нужен curl"
  mkdir -p "$OUT/Windows" "$OUT/Linux" "$OUT/macOS"

  # Список артефактов релиза: через gh, если есть, иначе — публичный API GitHub.
  #
  # ВАЖНО про черновики. `gh release list` показывает и ЧЕРНОВИКИ (владельцу репо),
  # а публичный API /releases/latest — нет. Без фильтра эти две ветки давали РАЗНЫЕ
  # релизы, и вариант с gh мог выбрать неопубликованный: файлы черновика по
  # browser_download_url недоступны, и сборка падала на curl с невнятным 404.
  # Поэтому: черновики и пререлизы отсеиваем, а явно указанный --version проверяем.
  note "Ищу релиз приложения…"
  if command -v gh >/dev/null; then
    TAG="${VERSION:-$(gh release list --repo "$REPO" --limit 30 \
      --json tagName,isDraft,isPrerelease \
      -q '[.[] | select(.isDraft == false and .isPrerelease == false)][0].tagName')}"
    [ -n "$TAG" ] && [ "$TAG" != "null" ] || die "не нашёл ни одного опубликованного релиза в $REPO"
    if [ "$(gh release view "$TAG" --repo "$REPO" --json isDraft -q '.isDraft')" = "true" ]; then
      die "релиз $TAG — ЧЕРНОВИК: его файлы нельзя скачать по прямой ссылке.
     Опубликуйте релиз на GitHub (Releases → Edit → Publish release) и повторите."
    fi
    URLS=$(gh release view "$TAG" --repo "$REPO" --json assets -q '.assets[].url')
  else
    API="https://api.github.com/repos/$REPO/releases/${VERSION:+tags/}${VERSION:-latest}"
    JSON=$(curl -fsSL "$API") || die "не удалось получить релиз ($API). Если это черновик — опубликуйте его."
    TAG=$(printf '%s' "$JSON" | grep -m1 '"tag_name"' | cut -d'"' -f4)
    URLS=$(printf '%s' "$JSON" | grep '"browser_download_url"' | cut -d'"' -f4)
  fi
  [ -n "$URLS" ] || die "у релиза $TAG нет артефактов (он опубликован?)"
  note "Релиз: $TAG"

  fetch() { # fetch <url> <куда>
    local name; name="$(basename "$1")"
    if [ -f "$2/$name" ]; then note "  есть: $name"; else
      note "  скачиваю: $name"
      curl -fL --progress-bar -o "$2/$name" "$1"
    fi
  }
  for u in $URLS; do
    case "$(basename "$u")" in
      *-setup.exe) fetch "$u" "$OUT/Windows" ;;
      *.deb|*.AppImage) fetch "$u" "$OUT/Linux" ;;
      *.dmg) fetch "$u" "$OUT/macOS" ;;
      *) : ;; # msi/rpm/sig/latest.json на флешку не кладём (не нужны установщику)
    esac
  done

  note "Скачиваю установщики движка Ollama…"
  fetch "$OLLAMA_WIN_URL" "$OUT/Windows"
  fetch "$OLLAMA_LINUX_URL" "$OUT/Linux"

  note "Кладу установочные скрипты и инструкцию…"
  cp "$USB_TPL/Установить.bat" "$OUT/Windows/"
  cp "$USB_TPL/install.sh" "$OUT/Linux/" && chmod +x "$OUT/Linux/install.sh" || true
  cp "$USB_TPL/ПРОЧТИ-МЕНЯ.html" "$OUT/"
fi

# ── Модели ───────────────────────────────────────────────────────────────────
if [ -n "$MODELS" ]; then
  command -v ollama >/dev/null || die "нужна установленная ollama (для pull и каталога моделей)"
  command -v jq >/dev/null || die "нужен jq (разбор манифестов): brew install jq / apt install jq"
  SRC="${OLLAMA_MODELS:-$HOME/.ollama/models}"
  DEST="$OUT/models"
  mkdir -p "$DEST/blobs" "$DEST/manifests"

  for tag in $MODELS; do
    note "Модель $tag: ollama pull…"
    ollama pull "$tag"

    # Путь манифеста: registry.ollama.ai/library/<имя>/<тег>; для hf.co — свой хост.
    name="${tag%%:*}"; ver="${tag#*:}"; [ "$name" = "$tag" ] && ver="latest"
    case "$name" in
      hf.co/*|huggingface.co/*) rel="manifests/$name/$ver" ;;
      */*) rel="manifests/registry.ollama.ai/$name/$ver" ;;
      *) rel="manifests/registry.ollama.ai/library/$name/$ver" ;;
    esac
    man="$SRC/$rel"
    [ -f "$man" ] || die "манифест не найден: $man (изменилась структура каталога Ollama?)"

    note "  копирую манифест и блобы…"
    mkdir -p "$DEST/$(dirname "$rel")"
    cp "$man" "$DEST/$rel"
    # Блобы модели: config + слои из манифеста (digest sha256:… → файл sha256-…).
    for digest in $(jq -r '.config.digest, .layers[].digest' "$man"); do
      blob="${digest/sha256:/sha256-}"
      if [ -f "$DEST/blobs/$blob" ]; then
        echo "    есть: $blob"
      else
        echo "    блоб: $blob ($(du -h "$SRC/blobs/$blob" | cut -f1))"
        cp "$SRC/blobs/$blob" "$DEST/blobs/$blob.part"
        mv "$DEST/blobs/$blob.part" "$DEST/blobs/$blob"
      fi
    done
  done
  note "Каталог моделей готов: $DEST"
fi

note "Готово. Проверьте, что флешка отформатирована в exFAT (блобы бывают >4 ГБ)."
