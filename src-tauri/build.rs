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

    // Пересобирать штамп при смене HEAD. Каталога .git может не быть (сборка из
    // архива) — тогда просто не подписываемся на изменения.
    if let Some(dir) = git(&["rev-parse", "--git-dir"]) {
        println!("cargo:rerun-if-changed={}/HEAD", dir.trim());
    }

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
