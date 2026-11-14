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

  # 默认识别后端：本包的 whisper-cli + 本包内的模型，开箱即用。
  # 用环境变量传（含空格），由 CMakeLists.txt 读入并转义后编进插件；
  # 走 cmakeFlags 会被 CMake 按空格拆成多个参数。
  # 用户可在 ~/.config/fcitx5/conf/voice.conf 或 FCITX5_VOICE_BACKEND 里改。
  preConfigure = ''
    cargo build --release --frozen --offline -p fcitx5-voice-core
    export VOICE_DEFAULT_BACKEND="${whisper-cpp}/bin/whisper-cli -m ${placeholder "out"}/share/fcitx5-voice/ggml-base.bin -l zh -nt -f"
  '';

  # 把模型装进本包（默认后端命令行里指向它）
  postInstall = ''
    install -Dm644 ${whisperModel} "$out/share/fcitx5-voice/ggml-base.bin"
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
    mainProgram = "fcitx5";
  };
}
