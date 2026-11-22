{
  description = "fcitx5-voice: 本地语音输入的 fcitx5 插件（默认按住右 Alt 说话）";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachSystem [ "x86_64-linux" "aarch64-linux" ] (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        voice = pkgs.callPackage ./nix/package.nix { };
      in
      {
        packages = {
          voice = voice;
          default = voice;
        };

        # 开发 shell：能直接 cargo test / cmake 构建
        devShells.default = pkgs.mkShell {
          inputsFrom = [ voice ];
          packages = with pkgs; [
            cargo
            rustc
            cargo-deny
            clang-tools
            cmake
            kdePackages.extra-cmake-modules
            pkg-config
            fcitx5
            alsa-lib
            sherpa-onnx
            python3
          ];
        };

        checks = {
          inherit voice;
          # 核心单测 + C ABI 行为测试
          tests =
            pkgs.stdenv.mkDerivation {
              name = "fcitx5-voice-tests";
              src = self;
              cargoDeps = pkgs.rustPlatform.importCargoLock {
                lockFile = ./Cargo.lock;
              };
              nativeBuildInputs = with pkgs; [
                rustPlatform.cargoSetupHook
                cargo
                rustc
                gcc
                pkg-config
              ];
              buildInputs = with pkgs; [
                alsa-lib
                # sherpa 协议测试用 python3 起假后端（streaming_channel_* 系列）
                python3
              ];
              # 核心单测 + C ABI 行为测试（capture 默认开，运行测试时跳过真实采集）
              buildPhase = ''
                runHook preBuild
                cargo test --offline --frozen -p fcitx5-voice-core
                cargo build --offline --frozen --release -p fcitx5-voice-core
                runHook postBuild
              '';
              doCheck = true;
              checkPhase = ''
                runHook preCheck
                g++ -std=c++20 test/key_harness.cpp -I core/include \
                  target/release/libfcitx5_voice_core.a -lasound -lpthread -o harness
                FCITX5_VOICE_TEST_NO_CAPTURE=1 ./harness
                runHook postCheck
              '';
              installPhase = ''
                touch $out
              '';
            };

          # 装好的插件能被 dlopen、且导出了 fcitx5 addon 工厂符号
          smoke =
            let
              p = self.packages.${system}.voice;
            in
            pkgs.runCommand "fcitx5-voice-smoke"
              {
                nativeBuildInputs = with pkgs; [ binutils coreutils gnugrep gnused ];
              }
              ''
                test -f ${p}/lib/fcitx5/libvoice.so
                test -f ${p}/share/fcitx5/addon/voice.conf
                grep -q '^Library=libvoice' ${p}/share/fcitx5/addon/voice.conf
                nm -D --defined-only ${p}/lib/fcitx5/libvoice.so \
                  | grep -q fcitx_addon_factory_instance

                # SenseVoice 模型 + tokens + 常駩识别器都在包里（运行期不下载）
                test -f ${p}/share/fcitx5-voice/sensevoice/model.int8.onnx
                test -f ${p}/share/fcitx5-voice/sensevoice/tokens.txt
                test "$(stat -c%s ${p}/share/fcitx5-voice/sensevoice/model.int8.onnx)" -gt 200000000
                test -x ${p}/libexec/fcitx5-voice/sherpa-serve

                # 默认配置必须用 stream: 前缀指向本包里的 sherpa-serve，
                # 且模型/ tokens 路径都是本包的（而不是 /usr/share 或要自己下）
                grep -q 'BackendCommand=stream:${p}/libexec/fcitx5-voice/sherpa-serve' \
                  ${p}/share/fcitx5/conf/voice.conf
                grep -q -- '--model=${p}/share/fcitx5-voice/sensevoice/model.int8.onnx' \
                  ${p}/share/fcitx5/conf/voice.conf
                grep -q -- '--tokens=${p}/share/fcitx5-voice/sensevoice/tokens.txt' \
                  ${p}/share/fcitx5/conf/voice.conf

                # 运行时能解析到 store 里的 sherpa-onnx C API 库
                ldd ${p}/libexec/fcitx5-voice/sherpa-serve \
                  | grep -q 'libsherpa-onnx-c-api'
                touch $out
              '';
        };

        # NixOS：
        #   services.xserver.desktopManager.fcitx5.addons = [
        #     inputs.fcitx5-voice.packages.${system}.default
        #   ];
        overlays.default = final: _prev: {
          fcitx5-voice = final.callPackage ./nix/package.nix { };
        };
      }
    );
}
