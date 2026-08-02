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
#     models-stt/ модель распознавания речи (ggml-small.bin) для голосового
#               ввода. Отдельно от models/ намеренно: это модель whisper, у неё
#               другой формат хранения, и в каталоге Ollama ей не место
#
# Части НЕЗАВИСИМЫ — можно заказать только инсталляторы, только модели, только голос:
#   tools/make-usb.sh --out /Volumes/JAI --installers
#   tools/make-usb.sh --out /Volumes/JAI --models "qwen3.5:9b bge-m3"
#   tools/make-usb.sh --out /Volumes/JAI --voice
#   tools/make-usb.sh --out /Volumes/JAI --installers --models "qwen3.5:9b qwen3.5:4b bge-m3" --voice
#
# Флаги:
#   --out DIR         куда собирать (корень флешки); ОБЯЗАТЕЛЬНЫЙ
#   --installers      скачать установщики приложения (последний релиз GitHub
#                     или --version app-vX.Y.Z) и движка Ollama + скрипты usb/
#   --models "TAGS"   вытянуть модели (ollama pull) и выборочно скопировать
#                     их blobs/manifests в models/ (нужны ollama и jq)
#   --voice           положить модель распознавания речи в models-stt/ (~465 МБ,
#                     скачивается с Hugging Face; нужен только curl)
#   --version TAG     конкретный тег релиза (по умолчанию — последний)
#
# ВАЖНО: флешку форматировать в exFAT — блоб модели 9B это ОДИН файл ~6 ГБ,
# FAT32 такие не хранит (лимит 4 ГБ). Объём: базовый набор — от 16 ГБ.
set -euo pipefail

REPO="baryshevshot1/jai"
# Версия движка зафиксирована: клиенту уезжает ровно та связка «приложение +
# Ollama», на которой шло тестирование, а прямая ссылка на релиз не ломается при
# смене схемы имён на ollama.com (так уже было: ollama-linux-amd64.tgz → .tar.zst).
# Поднимать осознанно, вместе с проверкой на целевом железе; версии в именах
# файлов движка нет, поэтому после подъёма удалите с флешки старые OllamaSetup.exe
# и ollama-linux-amd64.* — иначе они будут считаться уже скачанными.
OLLAMA_VER="v0.32.5"
OLLAMA_REL="https://github.com/ollama/ollama/releases/download/$OLLAMA_VER"
OLLAMA_WIN_URL="$OLLAMA_REL/OllamaSetup.exe"
OLLAMA_LINUX_URL="$OLLAMA_REL/ollama-linux-amd64.tar.zst"

# Модель распознавания речи (голосовой ввод): официальный репозиторий whisper.cpp
# на Hugging Face. Размер и sha256 зафиксированы — те же значения зашиты в
# приложение (src-tauri/src/voice.rs), поэтому битую копию оно отвергнет и здесь,
# и у клиента. Меняете модель — правьте ОБА места.
VOICE_URL="https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.bin"
VOICE_SIZE=487601967
VOICE_SHA="1be3a9b2063867b937e64e2ec7483364a79917e157fa98c5d94b5c1fffea987b"

OUT=""
DO_INSTALLERS=0
DO_VOICE=0
MODELS=""
VERSION=""

die() { echo "ошибка: $*" >&2; exit 1; }
note() { echo "── $*"; }
sha256_of() { # macOS — shasum, большинство Linux — sha256sum
  if command -v sha256sum >/dev/null; then sha256sum "$1"; else shasum -a 256 "$1"; fi | cut -d' ' -f1
}

while [ $# -gt 0 ]; do
  case "$1" in
    --out) OUT="${2:-}"; shift 2 ;;
    --installers) DO_INSTALLERS=1; shift ;;
    --models) MODELS="${2:-}"; shift 2 ;;
    --voice) DO_VOICE=1; shift ;;
    --version) VERSION="${2:-}"; shift 2 ;;
    -h|--help) sed -n '2,34p' "$0"; exit 0 ;;
    *) die "неизвестный флаг: $1 (см. --help)" ;;
  esac
done

[ -n "$OUT" ] || die "укажите --out <каталог флешки>"
[ "$DO_INSTALLERS" = 1 ] || [ -n "$MODELS" ] || [ "$DO_VOICE" = 1 ] \
  || die "нечего делать: добавьте --installers, --models и/или --voice"
mkdir -p "$OUT"

SELF_DIR="$(cd "$(dirname "$0")" && pwd)"
USB_TPL="$SELF_DIR/../usb"

# Скачивание файла на флешку: fetch <url> <каталог> [размер в байтах] [sha256]
# Общий для установщиков и модели распознавания.
# Во временное имя + mv: оборванная закачка не должна при перезапуске сойти за
# готовый файл и уехать на флешку (у клиента перекачать её нечем).
# -C -: докачка уцелевшего куска вместо старта с нуля.
fetch() {
  local name dst; name="$(basename "$1")"; dst="$2/$name"
  if [ -f "$dst" ]; then note "  есть: $name"; return 0; fi
  note "  скачиваю: $name"
  curl -fL --progress-bar -C - -o "$dst.part" "$1"
  if [ -n "${3:-}" ]; then
    local got; got=$(wc -c < "$dst.part")
    if [ "$got" -ne "$3" ]; then
      rm -f "$dst.part"
      die "размер $name: $got байт вместо $3 — файл битый, запустите сборку ещё раз"
    fi
  fi
  if [ -n "${4:-}" ] && [ "$(sha256_of "$dst.part")" != "${4#sha256:}" ]; then
    rm -f "$dst.part"
    die "sha256 $name не совпал с ожидаемым — файл записался с ошибкой, повторите"
  fi
  mv "$dst.part" "$dst"
}

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
    # «url размер sha256» — по ним после скачивания видно и обрыв, и тихую порчу
    # (digest у старых релизов бывает пустым, тогда остаётся сверка по размеру).
    ASSETS=$(gh release view "$TAG" --repo "$REPO" --json assets \
      -q '.assets[] | "\(.url) \(.size) \(.digest // "")"')
  else
    API="https://api.github.com/repos/$REPO/releases/${VERSION:+tags/}${VERSION:-latest}"
    JSON=$(curl -fsSL "$API") || die "не удалось получить релиз ($API). Если это черновик — опубликуйте его."
    TAG=$(printf '%s' "$JSON" | grep -m1 '"tag_name"' | cut -d'"' -f4)
    # Без gh размер и хэш грепом из JSON надёжно не достать — качаем без сверки.
    ASSETS=$(printf '%s' "$JSON" | grep '"browser_download_url"' | cut -d'"' -f4)
  fi
  [ -n "$ASSETS" ] || die "у релиза $TAG нет артефактов (он опубликован?)"
  note "Релиз: $TAG"

  # На флешке живёт РОВНО ОДНА версия приложения: старый установщик рядом с новым
  # заставляет Установить.bat / install.sh выбирать файл наугад (порядок каталога
  # на exFAT не гарантирован). Установщики Ollama не трогаем — версии в имени нет.
  VER="${TAG#app-v}"
  for d in "$OUT/Windows" "$OUT/Linux" "$OUT/macOS"; do
    for f in "$d"/Jarvis.AI_*; do
      [ -e "$f" ] || continue
      case "$(basename "$f")" in
        "Jarvis.AI_${VER}_"*) ;;
        *) note "  удаляю прошлую версию: $(basename "$f")"; rm -f "$f" ;;
      esac
    done
  done

  while read -r u size digest; do
    [ -n "$u" ] || continue
    case "$(basename "$u")" in
      *-setup.exe) fetch "$u" "$OUT/Windows" "${size:-}" "${digest:-}" ;;
      *.deb|*.AppImage) fetch "$u" "$OUT/Linux" "${size:-}" "${digest:-}" ;;
      *.dmg) fetch "$u" "$OUT/macOS" "${size:-}" "${digest:-}" ;;
      *) : ;; # msi/rpm/sig/latest.json на флешку не кладём (не нужны установщику)
    esac
  done <<< "$ASSETS"

  note "Скачиваю установщики движка Ollama ($OLLAMA_VER)…"
  fetch "$OLLAMA_WIN_URL" "$OUT/Windows"

  # Ollama раздаёт Linux-сборку только в .tar.zst, а zstd на машине клиента может
  # не оказаться — и поставить его офлайн неоткуда. Поэтому перепаковываем здесь,
  # на машине сборки: на флешку уезжает .tgz, который распаковывает штатный tar
  # без единой дополнительной программы. Нет zstd у нас — оставляем .tar.zst,
  # install.sh его тоже умеет и честно скажет клиенту, чего не хватает.
  #
  # Готовый .tgz проверяем ДО скачивания: иначе повторный запуск сборки заново тянул
  # бы гигабайтный .tar.zst (его-то мы после перепаковки удалили) и оставлял на
  # флешке сразу два архива движка.
  ZST="$OUT/Linux/ollama-linux-amd64.tar.zst"
  TGZ="$OUT/Linux/ollama-linux-amd64.tgz"
  if [ -f "$TGZ" ]; then
    note "  есть: ollama-linux-amd64.tgz"
    rm -f "$ZST" # с прошлых сборок: две копии движка флешке ни к чему
  else
    fetch "$OLLAMA_LINUX_URL" "$OUT/Linux"
    if command -v zstd >/dev/null 2>&1; then
      note "  перепаковываю движок в .tgz (клиенту не понадобится zstd)…"
      zstd -dc "$ZST" | gzip -1 > "$TGZ.part"
      mv "$TGZ.part" "$TGZ"
      rm -f "$ZST"
    else
      note "  ВНИМАНИЕ: нет zstd — на флешку уедет .tar.zst, и на машине клиента"
      note "  потребуется пакет zstd (офлайн его поставить будет неоткуда)."
    fi
  fi

  note "Кладу установочные скрипты и инструкцию…"
  # cmd.exe надёжен только с CRLF — приводим переводы строк независимо от того,
  # как файл лежит в рабочей копии (git мог отдать его и с LF).
  sed -e $'s/\r$//' -e $'s/$/\r/' "$USB_TPL/Установить.bat" > "$OUT/Windows/Установить.bat.part"
  mv "$OUT/Windows/Установить.bat.part" "$OUT/Windows/Установить.bat"
  cp "$USB_TPL/install.sh" "$OUT/Linux/"
  chmod +x "$OUT/Linux/install.sh" || true  # exFAT не хранит бит исполнения
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
        # Имя блоба — это его sha256: сверка копии ловит битую флешку сейчас,
        # а не у клиента невнятной ошибкой загрузки модели (перекачать там нечем).
        if [ "$(sha256_of "$DEST/blobs/$blob.part")" != "${blob#sha256-}" ]; then
          rm -f "$DEST/blobs/$blob.part"
          die "блоб $blob записался с ошибкой (sha256 не совпал) — проверьте носитель"
        fi
        mv "$DEST/blobs/$blob.part" "$DEST/blobs/$blob"
      fi
    done
  done
  note "Каталог моделей готов: $DEST"
fi

# ── Модель распознавания речи (голосовой ввод) ────────────────────────────────
# Кладём в ОТДЕЛЬНЫЙ каталог models-stt/: это модель whisper, а не Ollama, — в
# каталоге движка (models/) посторонний файл только мешал бы. Приложение ищет её
# там само («Настройки → Голосовой ввод → С флешки…»).
if [ "$DO_VOICE" = 1 ]; then
  command -v curl >/dev/null || die "нужен curl"
  note "Модель распознавания речи (~465 МБ)…"
  mkdir -p "$OUT/models-stt"
  fetch "$VOICE_URL" "$OUT/models-stt" "$VOICE_SIZE" "$VOICE_SHA"
  note "Модель распознавания готова: $OUT/models-stt"
fi

note "Готово. Проверьте, что флешка отформатирована в exFAT (блобы бывают >4 ГБ)."
