#!/usr/bin/env bash
# ── Установка Jarvis AI с флешки (Linux) ──────────────────────────────────────
# Ставит движок Ollama (в /usr/local, понадобится sudo) и приложение:
# .deb — на Debian/Ubuntu, иначе AppImage копируется в ~/Приложения.
# Модели затем импортирует само приложение: при первом запуске откроется
# «Мастер установки» и предложит каталог models с этой флешки.
#
# Запуск:  bash install.sh   (прямой запуск с флешки exFAT может не работать —
#                             у неё нет прав на исполнение, bash решает это)
set -euo pipefail
DIR="$(cd "$(dirname "$0")" && pwd)"
# set -e обрывает скрипт на первой ошибке (sudo tar, apt-get, cp) — но молча;
# объясняем человеку без терминального опыта, что установка НЕ завершилась.
trap 'echo; echo " Установка не завершена: ошибка на шаге выше. Откройте ПРОЧТИ-МЕНЯ.html в корне флешки." >&2' ERR

echo
echo " Установка Jarvis AI — офлайн-ассистента."
echo

# 1) Движок Ollama. Обычный случай — .tgz: make-usb.sh перепаковывает в него
# Linux-сборку (Ollama раздаёт .tar.zst), чтобы на этой машине не требовался
# zstd — офлайн его поставить было бы неоткуда. Ветка .tar.zst остаётся для
# флешек, записанных без перепаковки.
if command -v ollama >/dev/null 2>&1; then
  echo " [1/2] Движок Ollama уже установлен ($(ollama --version 2>/dev/null | head -1))."
elif [ -f "$DIR/ollama-linux-amd64.tgz" ]; then
  echo " [1/2] Движок Ollama → /usr/local (нужен sudo)…"
  sudo tar -C /usr/local -xzf "$DIR/ollama-linux-amd64.tgz"
  echo "       Установлен: $(/usr/local/bin/ollama --version 2>/dev/null | head -1)"
elif [ -f "$DIR/ollama-linux-amd64.tar.zst" ]; then
  # Без интернета сообщение должно быть точным, иначе клиенту некуда идти.
  command -v zstd >/dev/null 2>&1 \
    || { echo " [1/2] Для распаковки движка нужен zstd: sudo apt-get install zstd (dnf/pacman — по аналогии)." >&2; exit 1; }
  echo " [1/2] Движок Ollama → /usr/local (нужен sudo)…"
  zstd -dc "$DIR/ollama-linux-amd64.tar.zst" | sudo tar -C /usr/local -xf -
  echo "       Установлен: $(/usr/local/bin/ollama --version 2>/dev/null | head -1)"
else
  echo " [1/2] Архив движка Ollama не найден рядом со скриптом — пропускаю."
  echo "       Если движок не установлен: curl -fsSL https://ollama.com/install.sh | sh"
fi

# 2) Приложение. Рядом должна лежать ровно одна версия (make-usb.sh чистит
# старые): при нескольких файлах выбор «первого попавшегося» поставил бы
# клиенту произвольную версию — лучше честно остановиться.
for pat in "Jarvis.AI_*_amd64.deb" "Jarvis.AI_*_amd64.AppImage"; do
  n=$(find "$DIR" -maxdepth 1 -name "$pat" | wc -l)
  if [ "$n" -gt 1 ]; then
    echo " На флешке несколько файлов $pat — оставьте один (нужной версии) и запустите снова." >&2
    exit 1
  fi
done
DEB=$(ls "$DIR"/Jarvis.AI_*_amd64.deb 2>/dev/null | head -1 || true)
APPIMAGE=$(ls "$DIR"/Jarvis.AI_*_amd64.AppImage 2>/dev/null | head -1 || true)
# Установка AppImage — запасной путь. Вынесена в функцию, потому что нужна в ДВУХ
# случаях: когда .deb нет вовсе и когда .deb не поставился.
install_appimage() {
  # exFAT не хранит бит исполнения — AppImage обязан переехать на диск.
  local target="$HOME/Приложения"
  echo " [2/2] Приложение (AppImage) → $target…"
  mkdir -p "$target"
  cp "$APPIMAGE" "$target/"
  chmod +x "$target/$(basename "$APPIMAGE")"
  echo "       Запуск: $target/$(basename "$APPIMAGE")"
  echo "       (если не стартует — установите libfuse2: sudo apt install libfuse2)"
}

INSTALLED=""
if [ -n "$DEB" ] && command -v apt-get >/dev/null 2>&1; then
  echo " [2/2] Приложение (.deb, нужен sudo)…"
  # ВАЖНО: не даём apt уронить весь скрипт.
  #
  # Раньше здесь стоял голый `sudo apt-get install -y "$DEB"`, а AppImage был в
  # ветке `elif` — то есть рассматривался, только если .deb НЕ НАЙДЕН, а не если
  # он не поставился. Вместе с `set -e` и ERR-trap это означало: у клиента без
  # интернета, где apt не может дотянуть libwebkit2gtk, установка обрывалась
  # насмерть — при том что рядом на флешке лежал самодостаточный AppImage,
  # которому эти зависимости не нужны. Установка «под ключ» срывалась на ровном
  # месте, и владелец узнавал об этом уже у клиента.
  if sudo apt-get install -y "$DEB"; then
    INSTALLED="deb"
    echo "       Готово: ищите «Jarvis AI» в меню приложений."
  else
    echo
    echo "       Пакет .deb не поставился — обычно это нехватка системных"
    echo "       библиотек, которые без интернета не дотянуть."
    if [ -n "$APPIMAGE" ]; then
      echo "       Перехожу на запасной путь: AppImage, ему зависимости не нужны."
      echo
      install_appimage
      INSTALLED="appimage"
    fi
  fi
elif [ -n "$APPIMAGE" ]; then
  install_appimage
  INSTALLED="appimage"
fi

if [ -z "$INSTALLED" ]; then
  echo
  if [ -n "$DEB" ]; then
    echo " Приложение установить не удалось: .deb не поставился, а AppImage на"
    echo " флешке нет. Пересоберите флешку с AppImage (tools/make-usb.sh) —"
    echo " он ставится на любой дистрибутив и не требует интернета."
  else
    echo " [2/2] Не найден ни .deb, ни .AppImage рядом со скриптом."
  fi
  exit 1
fi

echo
echo " Готово! Запустите Jarvis AI. При первом запуске откроется «Мастер"
echo " установки» — он сам найдёт модели на этой флешке (папка models)"
echo " и предложит их импортировать на компьютер."
echo
