// ── Обновление приложения с диска (офлайн-путь) ─────────────────────────────

/// Обновление с диска (офлайн-путь): пользователь выбирает файл установщика новой
/// версии (принесён на флешке и т.п.), мы запускаем его штатным для ОС способом и
/// закрываем приложение, чтобы установщик мог заменить файлы. Сеть не используется.
/// В отличие от онлайн-канала (minisign-подпись updater'а) подпись файла здесь НЕ
/// проверяется — поэтому перед запуском пользователь явно предупреждается.
/// Путь в одинарных кавычках для подсказки-команды: путь к установщику приходит
/// из файлового диалога и может содержать пробелы и кириллицу. Команду мы НЕ
/// выполняем — её показывают пользователю, чтобы он мог скопировать её целиком.
#[cfg(target_os = "linux")]
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[tauri::command]
pub(crate) async fn install_update_from_disk(
    app: tauri::AppHandle,
    path: String,
) -> Result<(), String> {
    let p = std::path::PathBuf::from(&path);
    if !p.is_file() {
        return Err("Файл установщика не найден.".into());
    }
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    // Явное предупреждение: файл запускается без проверки подписи (в отличие от
    // онлайн-обновления). blocking_show нельзя звать на главном потоке — уводим в
    // пул блокирующих задач, диалог сам покажется где надо.
    let confirmed = {
        use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
        let app = app.clone();
        let file_name = p
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("установщик")
            .to_string();
        tauri::async_runtime::spawn_blocking(move || {
            app.dialog()
                .message(format!(
                    "Файл «{file_name}» будет запущен БЕЗ проверки подписи — приложение \
                     не может подтвердить, что он не подменён. Продолжайте, только если \
                     получили его из доверенного источника (официальная флешка или сайт).\n\n\
                     Запустить установку?"
                ))
                .title("Обновление с диска")
                .kind(MessageDialogKind::Warning)
                .buttons(MessageDialogButtons::OkCancelCustom(
                    "Установить".into(),
                    "Отмена".into(),
                ))
                .blocking_show()
        })
        .await
        .map_err(|e| format!("Не удалось показать предупреждение: {e}"))?
    };
    if !confirmed {
        return Err("Установка отменена.".into());
    }

    #[cfg(target_os = "windows")]
    match ext.as_str() {
        "msi" => {
            std::process::Command::new("msiexec")
                .arg("/i")
                .arg(&p)
                .spawn()
                .map_err(|e| format!("Не удалось запустить установщик: {e}"))?;
        }
        "exe" => {
            std::process::Command::new(&p)
                .spawn()
                .map_err(|e| format!("Не удалось запустить установщик: {e}"))?;
        }
        _ => return Err("Ожидается установщик Windows: файл .msi или .exe.".into()),
    }

    #[cfg(target_os = "linux")]
    match ext.as_str() {
        // Пакет отдаём системному обработчику — так не нужны права root внутри нашего
        // процесса. Но полагаться на него нельзя: в Ubuntu 22.04+ «Центр приложений»
        // .deb штатно не ставит, и xdg-open там либо открывает архиватор, либо молча
        // ничего не делает. Поэтому при неудаче возвращаем ТОЧНУЮ команду — клиенту
        // «ничего не произошло» хуже, чем строчка, которую можно передать установщику.
        "deb" | "rpm" => {
            let manual = if ext == "deb" {
                format!("sudo apt install {}", shell_quote(&path))
            } else {
                format!("sudo dnf install {}", shell_quote(&path))
            };
            // ДОЖИДАЕМСЯ кода возврата, а не факта запуска.
            //
            // Раньше здесь стоял `spawn().is_ok()` — это истинно всегда, когда сам
            // xdg-open найден в PATH, то есть практически всегда. Ветка с точной
            // командой не срабатывала НИКОГДА, а ниже приложение безусловно
            // закрывалось. На Ubuntu с GNOME открывался «Менеджер архивов» с
            // содержимым пакета, окно гасло, версия оставалась прежней — и человек
            // оставался без единой подсказки. xdg-open честно возвращает ненулевой
            // код, когда обработчика нет; надо было просто его прочитать.
            let status = std::process::Command::new("xdg-open")
                .arg(&p)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            let opened = matches!(&status, Ok(s) if s.success());
            if !opened {
                return Err(format!(
                    "Системный установщик пакетов не открылся. Установите обновление \
                     командой в терминале:\n{manual}"
                ));
            }
            // Пакетному менеджеру наше окно не мешает: он ставит файлы сам, и
            // закрывать приложение незачем. Гасили — и человек терял и окно, и
            // возможность прочитать, чем всё кончилось.
            return Ok(());
        }
        "appimage" => {
            return Err("AppImage обновляется заменой файла: закройте приложение и \
                        скопируйте новый AppImage поверх старого."
                .into());
        }
        _ => return Err("Ожидается пакет Linux: файл .deb или .rpm.".into()),
    }

    #[cfg(target_os = "macos")]
    match ext.as_str() {
        "dmg" | "app" => {
            std::process::Command::new("open")
                .arg(&p)
                .spawn()
                .map_err(|e| format!("Не удалось открыть образ: {e}"))?;
        }
        _ => return Err("Ожидается образ macOS: файл .dmg.".into()),
    }

    // Дать установщику стартовать и выйти самим — иначе он не сможет заменить файлы,
    // пока приложение их держит.
    //
    // ТОЛЬКО Windows и macOS. На Linux пакет ставит системный менеджер, наши файлы
    // ему не мешают, и закрывать окно незачем — а раньше оно закрывалось всегда, и
    // при неудачной установке человек оставался и без обновления, и без объяснения.
    // Ветка Linux выше возвращает результат сама, поэтому сюда не доходит.
    #[cfg(not(target_os = "linux"))]
    {
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(1200));
            app.exit(0);
        });
    }
    #[allow(unreachable_code)]
    Ok(())
}
