// Штамп сборки: версия, коммит и профиль попадают ВНУТРЬ бинарника.
//
// Зачем. Отладка голосового ввода однажды потеряла ход на том, что симптомы двух
// разных сборок сравнивались как симптомы одной: карточка «то есть, то нет» — это
// был старый бандл рядом с новым. Вопрос «а тот ли бандл я запустил» должен
// закрываться экраном «Диагностика», а не памятью.
//
// Ничего не скачиваем и ничего не требуем: если git недоступен (сборка из архива),
// в штампе честно окажется «неизвестно», а сборка не упадёт.

use std::process::Command;

fn main() {
    stamp("JAI_GIT_SHA", git(&["rev-parse", "--short=12", "HEAD"]));
    // Грязное дерево в релизной сборке — повод для тревоги, и это должно быть видно
    // в самом приложении, а не только в терминале того, кто собирал.
    let dirty = git(&["status", "--porcelain"]).map(|s| !s.trim().is_empty()).unwrap_or(false);
    stamp("JAI_GIT_DIRTY", Some(if dirty { "да" } else { "нет" }.to_string()));
    stamp("JAI_BUILD_PROFILE", std::env::var("PROFILE").ok());
    stamp("JAI_BUILD_TARGET", std::env::var("TARGET").ok());

    // Когда пересобирать штамп.
    //
    // Одного `.git/HEAD` НЕ хватает, и это уже подводило: при обычном коммите HEAD не
    // меняется — в нём лежит строка «ref: refs/heads/<ветка>», а новый sha пишется в
    // ФАЙЛ ВЕТКИ. В результате приложение отчитывалось предыдущим коммитом, то есть
    // штамп, заведённый ради вопроса «а тот ли бинарник я запустил», сам на него врал.
    //
    // Поэтому следим за тремя вещами: HEAD (переключение веток), файлом текущей ветки
    // (коммит, pull, reset) и packed-refs (ветка может жить там, а не отдельным файлом).
    // Каталога .git может не быть (сборка из архива) — тогда не подписываемся ни на что.
    if let Some(dir) = git(&["rev-parse", "--git-dir"]) {
        let dir = dir.trim();
        println!("cargo:rerun-if-changed={dir}/HEAD");
        println!("cargo:rerun-if-changed={dir}/packed-refs");
        if let Some(head_ref) = git(&["rev-parse", "--symbolic-full-name", "HEAD"]) {
            println!("cargo:rerun-if-changed={dir}/{}", head_ref.trim());
        }
    }
    // Признак «собрано с несохранёнными правками» точен настолько, насколько свежа
    // сама эта пересборка: правка файла перекомпилирует крейт, но НЕ перезапускает
    // build.rs. Гарантией чистоты дерева служит не он, а tools/release.sh, который
    // без чистого дерева собирать отказывается; здесь это подсказка, а не запрет.

    tauri_build::build()
}

fn stamp(key: &str, value: Option<String>) {
    let v = value.unwrap_or_else(|| "неизвестно".to_string());
    println!("cargo:rustc-env={key}={}", v.trim());
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}
