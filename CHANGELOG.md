# Changelog

## v0.2.1

**破坏性变更**：包内不再携带 whisper 模型。

v0.2.0 起默认后端已经是 SenseVoice，whisper 的 `ggml-base.bin`（141MB）只剩占体积。
这次把它连同 `whisper-cpp` 依赖一起从默认构建中移除。

### 移除

- `ggml-base.bin`（141MB）不再打包，`whisper-cpp` 不再是构建依赖。
  模型从 380MB（SenseVoice 239MB + whisper 141MB）降到 239MB。
- `packages.<system>.voice-whisper` 属性已删除（它本来就是「只带 whisper」的变体，
  现在没意义了）。**用了这个属性的 flake 会报错，改回 `.default` 即可。**
- `nix/package.nix` 的 `withWhisper` 参数已删除。

### 不变

- **通用的命令行后端能力完全保留**：`whisper-cli`、`sherpa-onnx-offline`、任何
  「读 WAV 路径、把文本打到 stdout」的命令仍然能用，只是引擎和模型要自己装，
  不再由本包代为打包。换成 whisper 的写法见 README 的「换识别后端」。

## v0.2.0

默认识别后端换成**常驻的 SenseVoice**，并修掉两个会导致「没反应」的严重问题。

### 新增

- **内置 SenseVoice 离线识别**（`sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17` int8），
  支持中 / 英 / 日 / 韩 / 粤，`--language=auto` 中英混说、`--itn=1` 逆文本规整。
- **`sherpa-serve`**：常驻识别器（`script/sherpa-serve.cpp`，直接调 sherpa-onnx C API）。
  模型只在第一次按键时加载，之后每次都是复用同一个进程，不再重复加载 239MB。
- 模型与 tokens **全部随包安装**，运行期不下载任何东西。
- `script/fetch-sensevoice-model.sh`：非 Nix 构建时下载模型（镜像优先、官方源兜底）。
- **包内不再携带 whisper 模型**：`ggml-base.bin`（141MB）与 `whisper-cpp` 依赖已从默认构建中
  移除（v0.2.0 起 SenseVoice 才是默认后端，whisper 那份只是占体积）。想用 whisper 请自行安装
  `whisper-cli` 与模型，再用 `BackendCommand` 指过去——通用的命令行后端能力仍然保留。

### 修复

- **识别期间整个输入法卡死**：`voice_key_up` 原先在主线程里同步等外部识别进程
  （最长十几秒）。现在识别挪到后台线程，主线程只轮询，松开按键立即返回。
- **说长句不上屏**：`sherpa-serve` 原先每 2 秒解码后清空缓冲，导致句末只识别了最后
  不到 2 秒的碎片（常常是空串）。现在句末用**整句**音频完整解码一次。
- `StreamingChannel::reset` 不再重复发句末哨兵，避免污染下一句。
- 句末结果为空时退回预览文本，避免尾巴是静音时丢掉已识别的内容。
- 低于 300ms 的录音直接跳过，不再为误触白白启动识别进程。
- 删除错误的 `meta.mainProgram` / `apps.default`（指向依赖里的 `fcitx5`，不是本包产物）。
- 补齐 `LICENSE-MIT` / `LICENSE-APACHE`，与 `Cargo.toml` 的 `MIT OR Apache-2.0` 一致。

### 变更

- 默认识别后端由 `whisper-cli` 改为 `stream:.../sherpa-serve`。
  想换回 whisper：自行安装 `whisper-cli` 与模型后改 `BackendCommand`。
- `stream:` 协议新增句末哨兵（4 字节 `FF FF FF FF`），进程可跨句复用；
  不认识哨兵的旧 wrapper 仍能靠关闭 stdin 收尾（有 1.5s 宽限兜底）。

### 已知限制

- SenseVoice 是离线模型，**分段出字**（每新增 2 秒重解整句并覆盖输出），不是逐字流式。
- 独立 fcitx5 addon 无法写真正的 preedit，实时预览只能靠逐字刷。

## v0.1.0

首个版本：按住右 Alt 说话、松手上屏；C++ 插件 + Rust 核心；
支持 whisper.cpp / sherpa-onnx / 自定义命令行后端；Nix 包自带 whisper `ggml-base` 模型。
