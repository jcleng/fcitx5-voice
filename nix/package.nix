# fcitx5-voice：本地语音输入的 fcitx5 插件。
#
# 结构：Rust 核心（core/）静态链接进 C++ 插件（src/），插件只在
# PreInputMethod 阶段拦截语音键，其余按键交给当前输入法（rime/pinyin）。
#
# 默认后端是常驻的 sherpa-serve（SenseVoice int8），模型随包安装；
# 想换回 whisper.cpp 只需改 BackendCommand。
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
  sherpa-onnx,
  withWhisper ? true,
  withSenseVoice ? true,
}:

let
  version = (lib.importTOML ../Cargo.toml).workspace.package.version;

  # whisper.cpp 的 base 多语种模型（约 141MB）。按 hash 固定，构建时从 Hugging Face 拉。
  # 中文够用、速度也快；想更准可以换 ggml-small.bin（约 466MB），改 url/hash 即可。
  whisperModel = fetchurl {
    url = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.bin";
    hash = "sha256-YO1bw90U7qhWST0zQ0m0BXgt3K8AKNS130CINF+6Lv4=";
  };

  # sherpa-onnx 的 SenseVoice int8（zh/en/ja/ko/yue，约 228MB）+ tokens.txt。
  #
  # 用 Hugging Face 官方源而不是 hf-mirror：fetchurl 没有「失败换下一个」的能力，
  # 镜像一旦内容变动 hash 就崩。镜像适合运行期脚本（见 script/fetch-sensevoice-model.sh），
  # 不适合写进 Nix 的固定输出路径。
  senseVoiceModel = fetchurl {
    url = "https://huggingface.co/csukuangfj/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17/resolve/main/model.int8.onnx";
    hash = "sha256-xx8M4AvslbB3ROEWNF4z2Mu+CM74ljgs+Qe/S1GizVE=";
  };
  senseVoiceTokens = fetchurl {
    url = "https://huggingface.co/csukuangfj/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17/resolve/main/tokens.txt";
    hash = "sha256-9EnrKNxWdTPX+lm+NOKryoeE93GFDHikf7cxoxQpodw=";
  };

  # 默认识别命令。有 SenseVoice 就用常驻的 sherpa-serve（只加载一次模型），
  # 否则退回 whisper-cli。
  defaultBackend =
    if withSenseVoice then
      "stream:${placeholder "out"}/libexec/fcitx5-voice/sherpa-serve"
      + " --model=${placeholder "out"}/share/fcitx5-voice/sensevoice/model.int8.onnx"
      + " --tokens=${placeholder "out"}/share/fcitx5-voice/sensevoice/tokens.txt"
      + " --language=auto --itn=1"
    else if withWhisper then
      "${whisper-cpp}/bin/whisper-cli -m ${placeholder "out"}/share/fcitx5-voice/ggml-base.bin -l zh -nt -f"
    else
      "";
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
      ../script
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
  ]
  ++ lib.optional withSenseVoice sherpa-onnx;

  cmakeFlags = [
    # fcitx5 的 CMake 配置里这个目录指向 fcitx5 自己的 store 路径，要装进本包
    "-DFCITX_INSTALL_PKGDATADIR=${placeholder "out"}/share/fcitx5"
    "-DVOICE_SHERPA_SERVE=${if withSenseVoice then "ON" else "OFF"}"
  ];

  # 插件静态链接 Rust 核心：CMake 配置时 target/release/libfcitx5_voice_core.a 必须已在
  # 同时探测 sherpa-onnx 的真实头/库路径（库名可能带版本号）。
  preConfigure = ''
    cargo build --release --frozen --offline -p fcitx5-voice-core --features capture
    ${lib.optionalString withSenseVoice ''
      # 头文件目录：c-api.h 在 <include>/sherpa-onnx/c-api/c-api.h
      if [ -f ${sherpa-onnx}/include/sherpa-onnx/c-api/c-api.h ]; then
        SV_INC=${sherpa-onnx}/include
      elif [ -f ${sherpa-onnx}/include/c-api.h ]; then
        SV_INC=${sherpa-onnx}/include
      else
        echo "找不到 sherpa-onnx/c-api.h" >&2
        exit 1
      fi
      # 库：c-api 里的 SherpaOnnxCreateOfflineRecognizer 在 libsherpa-onnx-c-api
      # （上游 install(TARGETS sherpa-onnx-c-api ...) 装成这个名字，没有 libsherpa-onnx.so）
      # 用 shell glob 而不是 find|head，避免 pipefail 下 head 提前退出导致失败
      SV_LIB=""
      for f in ${sherpa-onnx}/lib/libsherpa-onnx-c-api.so*; do
        [ -e "$f" ] && { SV_LIB="$f"; break; }
      done
      if [ -z "$SV_LIB" ]; then
        echo "找不到 libsherpa-onnx.so" >&2
        exit 1
      fi
      echo "sherpa-onnx include=$SV_INC lib=$SV_LIB"
      export SHERPA_ONNX_INCLUDE_DIR="$SV_INC"
      export SHERPA_ONNX_LIBRARY="$SV_LIB"
    ''}
  '';

  # 资源全部随包安装，运行期不下载任何东西。
  postInstall = ''
    ${lib.optionalString withWhisper ''
      install -Dm644 ${whisperModel} "$out/share/fcitx5-voice/ggml-base.bin"
    ''}
    ${lib.optionalString withSenseVoice ''
      install -Dm644 ${senseVoiceModel} \
        "$out/share/fcitx5-voice/sensevoice/model.int8.onnx"
      install -Dm644 ${senseVoiceTokens} \
        "$out/share/fcitx5-voice/sensevoice/tokens.txt"
    ''}

    # 默认识别命令：直接改写安装好的 conf 文本。
    # 不走 -D：后端命令行带空格，而 cmakeFlags 是逐元素直传给 cmake 的
    # （不经 shell），CMake 的 -D 取值既不认引号也不能带空格。
    # 这里匹配 CMakeLists 里的默认值（Nix 下没覆盖 -D，所以就是它）并整行替掉。
    CONF="$out/share/fcitx5/conf/voice.conf"
    substituteInPlace "$CONF" \
      --replace-fail 'BackendCommand=whisper-cli -m /usr/share/whisper/ggml-base.bin -l zh -nt -f' \
      ${lib.escapeShellArg "BackendCommand=${defaultBackend}"}
  '';

  # sherpa-serve 依赖 store 里的 libsherpa-onnx.so，装好后得能找到它
  dontPatchELF = false;

  passthru = {
    inherit
      whisper-cpp
      whisperModel
      sherpa-onnx
      senseVoiceModel
      senseVoiceTokens
      ;
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
