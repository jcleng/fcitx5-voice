# Changelog

## v0.2.0

默认识别后端换成**常驻的 SenseVoice**，并修掉两个会导致「没反应」的严重问题。

### 新增

- **内置 SenseVoice 离线识别**（`sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17` int8），
  支持中 / 英 / 日 / 韩 / 粤，`--language=auto` 中英混说、`--itn=1` 逆文本规整。
- **`sherpa-serve`**：常驻识别器（`script/sherpa-serve.cpp`，直接调 sherpa-onnx C API）。
  模型只在第一次按键时加载，之后每次都是复用同一个进程，不再重复加载 239MB。
- 模型与 tokens **全部随包安装**，运行期不下载任何东西。
- 新增 `packages.<system>.voice-whisper`：只带 whisper 的精简变体（省约 230MB），
  供系统没装 sherpa-onnx 或只想用 whisper 的场景。
- `script/fetch-sensevoice-model.sh`：非 Nix 构建时下载模型（镜像优先、官方源兜底）。

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
  想换回 whisper：改 `BackendCommand` 或装 `voice-whisper`。
- `stream:` 协议新增句末哨兵（4 字节 `FF FF FF FF`），进程可跨句复用；
  不认识哨兵的旧 wrapper 仍能靠关闭 stdin 收尾（有 1.5s 宽限兜底）。

### 已知限制

- SenseVoice 是离线模型，**分段出字**（每新增 2 秒重解整句并覆盖输出），不是逐字流式。
- 独立 fcitx5 addon 无法写真正的 preedit，实时预览只能靠逐字刷。

## v0.1.0

首个版本：按住右 Alt 说话、松手上屏；C++ 插件 + Rust 核心；
支持 whisper.cpp / sherpa-onnx / 自定义命令行后端；Nix 包自带 whisper `ggml-base` 模型。
