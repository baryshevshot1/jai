@echo off
chcp 65001 >nul
rem ── Установка Jarvis AI с флешки (Windows) ──────────────────────────────────
rem Ставит движок Ollama и приложение тихо, по очереди (обе установки per-user,
rem права администратора не нужны). Модели затем импортирует само приложение:
rem при первом запуске откроется «Мастер установки» и предложит каталог models
rem с этой флешки.
setlocal
cd /d "%~dp0"

echo.
echo  Установка Jarvis AI — офлайн-ассистента.
echo.

if exist "OllamaSetup.exe" (
  echo  [1/2] Движок Ollama... ^(1-2 минуты^)
  start /wait "" "OllamaSetup.exe" /VERYSILENT /NORESTART
  if errorlevel 1 (
    echo        Установщик движка завершился с ошибкой.
    goto :fail
  )
) else (
  echo  [1/2] OllamaSetup.exe не найден рядом со скриптом — пропускаю.
  echo        Если движок не установлен, скачайте его с ollama.com
)

rem Рядом должна лежать ровно одна версия (make-usb.sh чистит старые):
rem порядок перечисления каталога на exFAT не гарантирован, и «последний
rem найденный» файл ставил бы клиенту произвольную версию.
set "APP="
set /a APPCOUNT=0
for %%f in ("Jarvis.AI_*-setup.exe") do (
  set "APP=%%f"
  set /a APPCOUNT+=1
)
if %APPCOUNT% GTR 1 (
  echo  На флешке несколько установщиков Jarvis.AI — оставьте один ^(нужной версии^) и запустите снова.
  goto :fail
)
if defined APP (
  echo  [2/2] Приложение Jarvis AI...
  start /wait "" "%APP%" /S
  if errorlevel 1 (
    echo        Установщик приложения завершился с ошибкой.
    goto :fail
  )
) else (
  echo  [2/2] Установщик Jarvis.AI_*-setup.exe не найден рядом со скриптом.
  goto :fail
)

echo.
echo  Готово! Запустите Jarvis AI из меню «Пуск».
echo  При первом запуске откроется «Мастер установки» — он сам найдёт модели
echo  на этой флешке (папка models) и предложит их импортировать.
echo.
pause
exit /b 0

:fail
echo.
echo  Установка не завершена. Откройте ПРОЧТИ-МЕНЯ.html в корне флешки.
echo.
pause
exit /b 1
