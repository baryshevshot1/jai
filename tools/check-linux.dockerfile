# Проверка сборки под Linux на машине разработчика (macOS).
#
# Зачем: Tauri не кросс-компилируется, а ветки #[cfg(target_os = "linux")] на Mac не
# компилируются вовсе — их ошибки видны только на самом Linux. Раньше это ловил CI;
# когда он недоступен, тот же прогон делается здесь, в контейнере.
#
# Образ повторяет окружение CI: та же версия Rust (rust-toolchain.toml) и тот же набор
# системных библиотек, что ставит ci.yml.
FROM rust:1.96.0-bookworm

# Системные зависимости Tauri 2 под Linux — ровно те же, что в ci.yml.
# libwebkit2gtk — webview, librsvg — иконки, patchelf — правка RPATH в бандле.
RUN apt-get update && apt-get install -y --no-install-recommends \
      libwebkit2gtk-4.1-dev \
      libappindicator3-dev \
      librsvg2-dev \
      patchelf \
      build-essential \
      curl \
      file \
      pkg-config \
      libssl-dev \
    && rm -rf /var/lib/apt/lists/*

RUN rustup component add clippy rustfmt

WORKDIR /work
