# fcitx5-voice：本地语音输入的 fcitx5 插件。
#
# 结构：Rust 核心（core/）静态链接进 C++ 插件（src/），插件只在
# PreInputMethod 阶段拦截语音键，其余按键交给当前输入法（rime/pinyin）。
{
  lib,
  stdenv,
  fetchurl,
  rustPlatform,
  cargo,
  rustc,
  cmake,
  kdePackages,
  pkg-config,
  fcitx5,
  alsa-lib,
  whisper-cpp,
}:

let
  version = (lib.importTOML ../Cargo.toml).workspace.package.version;

  # whisper.cpp 的 base 多语种模型（约 141MB）。按 hash 固定，构建时从 Hugging Face 拉。
  # 中文够用、速度也快；想更准可以换 ggml-small.bin（约 466MB），改 url/hash 即可。
  whisperModel = fetchurl {
    url = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.bin";
    hash = "sha256-YO1bw90U7qhWST0zQ0m0BXgt3K8AKNS130CINF+6Lv4=";
  };
in
stdenv.mkDerivation {
  pname = "fcitx5-voice";
  inherit version;

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../core
      ../src
      ../conf
      ../test
      ../CMakeLists.txt
      ../LICENSE-MIT
      ../LICENSE-APACHE
    ];
  };

  cargoDeps = rustPlatform.importCargoLock { lockFile = ../Cargo.lock; };

  nativeBuildInputs = [
    rustPlatform.cargoSetupHook
    cargo
    rustc
    cmake
    kdePackages.extra-cmake-modules
    pkg-config
  ];

  buildInputs = [
    fcitx5
    alsa-lib
  ];

  cmakeFlags = [
    # fcitx5 的 CMake 配置里这个目录指向 fcitx5 自己的 store 路径，要装进本包
    "-DFCITX_INSTALL_PKGDATADIR=${placeholder "out"}/share/fcitx5"
  ];

  # 插件静态链接 Rust 核心：CMake 配置时 target/release/libfcitx5_voice_core.a 必须已在
  preConfigure = ''
    cargo build --release --frozen --offline -p fcitx5-voice-core --features capture
  '';

  # 1) 装模型；2) 把默认识别命令写进纯文本配置（不走二进制替换，避免
  #    Nix placeholder 长度变化导致的字符串错位）。
  postInstall = ''
    install -Dm644 ${whisperModel} "$out/share/fcitx5-voice/ggml-base.bin"

    substituteInPlace "$out/share/fcitx5/conf/voice.conf" \
      --replace-fail 'BackendCommand=whisper-cli -m /usr/share/whisper/ggml-base.bin -l zh -nt -f' \
                     "BackendCommand=${whisper-cpp}/bin/whisper-cli -m ${placeholder "out"}/share/fcitx5-voice/ggml-base.bin -l zh -nt -f"
  '';

  passthru = {
    inherit whisper-cpp whisperModel;
  };

  meta = {
    description = "Local voice input for fcitx5: hold Right Alt to dictate, release to commit";
    homepage = "https://github.com/jcleng/fcitx5-voice";
    license = with lib.licenses; [
      mit
      asl20
    ];
    platforms = lib.platforms.linux;
  };
}
