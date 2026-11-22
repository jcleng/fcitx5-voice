//! sherpa-onnx 后端。
//!
//! ## 为什么这里不是真流式
//!
//! sherpa-onnx 确实有能流式的 `Online*` 模型（streaming-zipformer / streaming-paraformer），
//! 但**它的命令行工具只吃 WAV 文件路径，不能从 stdin 持续读 PCM**：
//!
//! - `sherpa-onnx`（流式 CLI，`csrc/sherpa-onnx.cc`）要求至少 1 个 WAV 文件参数，
//!   `po.NumArgs() < 1` 直接报错；音频全部走 `ReadWave(filename)`，没有 `-` / `/dev/stdin`
//!   之类的约定。
//! - `sherpa-onnx-vad-with-online-asr`（VAD + 流式模型）同样要求「恰好 1 个 wav 文件」。
//! - `OnlineRecognizerConfig` 也没有 `--print-partial` / `--print-progress` 这类选项，
//!   结果只在解码结束后一次性打到 stderr。
//!
//! 所以本模块的 `SherpaChannel` 是**离线**实现：整段 PCM 写 WAV、调 CLI、读文本。
//! 它的 `partial()` 恒为 `None`，不会边说边出字。
//!
//! 要真正流式只有两条路，都在 [`Backend`] 里留了位置：
//! - [`Backend::Streaming`]：走一个**支持 stdin 流式**的外部命令（协议见
//!   [`StreamingChannel`]），将来可以自己写个小 wrapper 或用别的引擎；
//! - 直接调 sherpa-onnx 的 C API（`SherpaOnnxOnlineStreamAcceptWaveform` 等）。
//!   维护成本高、需要 unsafe FFI，当前不实现。

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};

use crate::asr::{split_command, AsrChannel};

/// 识别后端类型，由 `BackendCommand` 的前缀决定。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Backend {
    /// 通用「写到 WAV 再调命令行」后端（whisper-cli、sherpa-onnx-offline 等）
    Process(String),
    /// sherpa-onnx CLI（离线，`partial()` 为 None）
    Sherpa(SherpaOptions),
    /// 支持 stdin 流式协议的外部命令（真 `partial()`）
    Streaming(String),
}

/// `sherpa-streaming:` / `sherpa:` 前缀后面的参数
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SherpaOptions {
    /// 识别命令（不含 WAV 路径，路径由渠道追加）
    pub command: Vec<String>,
}

impl SherpaOptions {
    /// 从命令行字符串解析。
    ///
    /// 空串返回空的 `command`（表示「没配」）；非空时按 shell 规则拆词。
    pub fn parse(cmdline: &str) -> Self {
        Self {
            command: split_command(cmdline),
        }
    }

    /// 补全后的完整命令：第一个词是选项（以 `-` 开头）或整个为空时，
    /// 前面补 `sherpa-onnx-offline`，这样用户可以直接写 `--tokens=...`。
    pub fn program(&self) -> Vec<String> {
        match self.command.first() {
            Some(first) if !first.starts_with('-') => self.command.clone(),
            _ => {
                let mut out = vec!["sherpa-onnx-offline".to_string()];
                out.extend(self.command.iter().cloned());
                out
            }
        }
    }

    /// 去掉命令里已经写死的 `--wav=` 参数（渠道会自己追加路径）
    pub fn command_without_wav(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut skip_next = false;
        for arg in &self.command {
            if skip_next {
                skip_next = false;
                continue;
            }
            if arg == "--wav" {
                skip_next = true;
                continue;
            }
            if arg.starts_with("--wav=") {
                continue;
            }
            out.push(arg.clone());
        }
        out
    }

    /// 最终要执行的命令：补全程序名，并去掉用户写死的 `--wav`。
    pub fn resolved_command(&self) -> Vec<String> {
        let stripped = SherpaOptions {
            command: self.command_without_wav(),
        };
        stripped.program()
    }
}

impl Backend {
    /// 按前缀识别后端类型。没有前缀的一律当 `Process`（向后兼容）。
    ///
    /// - `sherpa-streaming:<cmd>` → sherpa 命令行（仍然离线，见模块文档）
    /// - `sherpa-offline:<cmd>` / `sherpa:<cmd>` → 同上
    /// - `stream:<cmd>` → stdin 流式协议（真流式）
    /// - 其它 → `Process`
    pub fn parse(cmd: &str) -> Backend {
        let cmd = cmd.trim();
        for (prefix, kind) in [
            ("sherpa-streaming:", Kind::Sherpa),
            ("sherpa-offline:", Kind::Sherpa),
            ("sherpa:", Kind::Sherpa),
            ("stream:", Kind::Streaming),
        ] {
            if let Some(rest) = cmd.strip_prefix(prefix) {
                return match kind {
                    Kind::Sherpa => Backend::Sherpa(SherpaOptions::parse(rest)),
                    Kind::Streaming => Backend::Streaming(rest.trim().to_string()),
                };
            }
        }
        Backend::Process(cmd.to_string())
    }

    /// 造出对应的识别渠道；命令为空时返回 None。
    pub fn build_channel(&self) -> Option<Box<dyn AsrChannel>> {
        match self {
            Backend::Process(cmd) if cmd.is_empty() => None,
            Backend::Process(cmd) => Some(Box::new(crate::asr::ProcessChannel::new("process", cmd))),
            Backend::Sherpa(opts) if opts.command.is_empty() => None,
            Backend::Sherpa(opts) => Some(Box::new(SherpaChannel::new("sherpa", opts.clone()))),
            Backend::Streaming(cmd) if cmd.is_empty() => None,
            Backend::Streaming(cmd) => Some(Box::new(StreamingChannel::new("stream", cmd))),
        }
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Sherpa,
    Streaming,
}

/// sherpa-onnx 命令行后端（离线）。
///
/// 行为与 [`crate::asr::ProcessChannel`] 一致：攒 PCM、结束时写 WAV、调 CLI、取文本。
/// 区别是它会去掉用户命令里写死的 `--wav=`，避免把路径重复传一次。
pub struct SherpaChannel {
    name: String,
    opts: SherpaOptions,
    pcm: Vec<i16>,
}

/// 太短的片段不值得起一个识别进程。
///
/// 参照 trime-bibi-keyboard 的 `SenseVoiceEngine.MIN_AUDIO_MS`：按住语音键误触
/// 几十毫秒的片段送去解码只会得到空串或噪声，而 SenseVoice 这类大模型每次
/// 都要重新加载几百 MB，白等几秒。300ms 以下的录音直接跳过。
pub const MIN_AUDIO_MS: usize = 300;

/// 每次识别至少要有的采样点数（16kHz 单声道）。
pub const MIN_SAMPLES: usize = 16000 * MIN_AUDIO_MS / 1000;

impl SherpaChannel {
    pub fn new(name: impl Into<String>, opts: SherpaOptions) -> Self {
        Self {
            name: name.into(),
            opts,
            pcm: Vec::new(),
        }
    }

    /// 最终文本：优先 stderr，空则退回 stdout。
    ///
    /// sherpa-onnx 的 CLI 把识别结果打到 **stderr**（`std::cerr << os.str()`），
    /// stdout 只有 "Start to create recognizer" 这类进度行，所以两条都要看。
    fn run(&self) -> Option<String> {
        // 太短就不必起进程了（见 MIN_AUDIO_MS）
        if self.pcm.len() < MIN_SAMPLES {
            return None;
        }
        let path = temp_wav_path();
        if crate::wav::write_wav(&path, &self.pcm).is_err() {
            return None;
        }
        let command = self.opts.resolved_command();
        let bin = command.first()?.clone();
        let out = Command::new(&bin)
            .args(&command[1..])
            .arg(&path)
            .output()
            .ok();
        let _ = std::fs::remove_file(&path);
        let out = out?;
        if !out.status.success() {
            return None;
        }
        let text = pick_text(
            &String::from_utf8_lossy(&out.stdout),
            &String::from_utf8_lossy(&out.stderr),
        )?;
        Some(crate::t2s::to_simplified(&text))
    }
}

impl AsrChannel for SherpaChannel {
    fn name(&self) -> &str {
        &self.name
    }

    fn feed(&mut self, pcm: &[i16]) {
        self.pcm.extend_from_slice(pcm);
    }

    fn finish(&mut self) -> Option<String> {
        let text = self.run();
        self.pcm.clear();
        text
    }

    fn reset(&mut self) {
        self.pcm.clear();
    }
}

/// 从 sherpa-onnx 的输出里挑出识别文本。
///
/// CLI 的输出形状（stderr）：
/// ```text
/// Number of threads: 2, Elapsed seconds: 0.31, Audio duration (s): 3.20, Real time factor (RTF) = ...
/// 今天天气不错
/// {"text": "今天天气不错", "tokens": [...], ...}
/// ```
/// 规则：跳过 RTF 统计行和 `{...}` JSON 行，取剩下的第一行非空文本。
pub fn pick_text(stdout: &str, stderr: &str) -> Option<String> {
    for src in [stderr, stdout] {
        for line in src.lines() {
            let t = line.trim();
            if t.is_empty() || is_noise_line(t) {
                continue;
            }
            return Some(t.to_string());
        }
    }
    None
}

/// 是否为 CLI 的非识别文本行
fn is_noise_line(line: &str) -> bool {
    line.starts_with('{')
        || line.starts_with("Number of threads:")
        || line.starts_with("Elapsed seconds:")
        || line.starts_with("Start to create recognizer")
        || line.starts_with("Recognizer created")
        || line.starts_with("OnlineRecognizerConfig(")
        || line.starts_with("FeatureExtractorConfig(")
        || line.starts_with("OnlineModelConfig(")
        || line.starts_with("EndpointConfig(")
        || line.starts_with("HomophoneReplacerConfig(")
        || line.starts_with("lm_config")
        || line.starts_with("model_config")
        || line.starts_with("feat_config")
        || line.starts_with("endpoint_config")
        || line.ends_with(".wav")
}

fn temp_wav_path() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("fcitx5-voice-sherpa-{}.wav", std::process::id()))
}

/// 支持 stdin 流式协议的外部命令渠道。
///
/// 协议（约定，不是 sherpa-onnx 的）：子进程从 stdin 读裸 16kHz 单声道 `i16` 小端 PCM，
/// 每读到一段就向 stdout 输出**当前整句的中间结果**，一行一次（覆盖式，不是增量）。
///
/// ## 句末标记
///
/// 早期版本用「关闭 stdin」表示说完。但那样每次按键都要重新起进程、
/// 重新加载模型（SenseVoice int8 约 239MB），首字延迟很挤。
/// 现在改为：句末往 stdin 写 4 字节哨兵 `0xFF 0xFF 0xFF 0xFF`
/// （作为 i16 = -1，真实麦克风不会给出这个值），子进程据此切句并输出最终行，
/// **进程和模型继续保留**，下次按键直接复用。
///
/// 兼容：不认识哨兵的子进程会把它当普通音频，行为退化为「只到 EOF 才出结果」——
/// 那种情况仍然由 `stop()` 关 stdin 兵底。所以旧 wrapper 不会挂。
pub struct StreamingChannel {
    name: String,
    program: Vec<String>,
    /// 子进程（跨会话复用）
    child: Option<Child>,
    /// 往子进程 stdin 写 PCM 的句柄
    stdin: Option<ChildStdin>,
    /// 读线程刷新的「当前中间结果」
    partial: Arc<Mutex<String>>,
    /// 收集读线程看到的最后一行（final 用）
    last_line: Arc<Mutex<String>>,
    /// 读线程句柄（`stop` 时等它读完最后一行）
    reader: Option<std::thread::JoinHandle<()>>,
    /// 子进程是否已报告「本句结束」（收到一个空行）
    finished: Arc<Mutex<bool>>,
}

/// 句末哨兵：4 字节，解成 i16 是 -1（真实麦克风不会给出这个值）
const SENTINEL: [u8; 4] = [0xFF, 0xFF, 0xFF, 0xFF];

impl StreamingChannel {
    pub fn new(name: impl Into<String>, cmdline: &str) -> Self {
        Self {
            name: name.into(),
            program: split_command(cmdline),
            child: None,
            stdin: None,
            partial: Arc::new(Mutex::new(String::new())),
            last_line: Arc::new(Mutex::new(String::new())),
            reader: None,
            finished: Arc::new(Mutex::new(false)),
        }
    }

    /// 启动子进程并起后台读线程
    fn ensure_started(&mut self) -> bool {
        if self.child.is_some() {
            return true;
        }
        let Some((bin, args)) = self.program.split_first() else {
            return false;
        };
        let spawned = Command::new(bin)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let Ok(mut child) = spawned else {
            return false;
        };
        let stdin = child.stdin.take();
        let partial = self.partial.clone();
        let last_line = self.last_line.clone();
        let finished = self.finished.clone();
        if let Some(stdout) = child.stdout.take() {
            // 读线程只碰几个 Arc<Mutex<..>>，天然 Send
            self.reader = Some(std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    let Ok(line) = line else { break };
                    let line = line.trim().to_string();
                    if line.is_empty() {
                        // 空行 = sherpa-serve 告知「本句结束、无文本」
                        if let Ok(mut f) = finished.lock() {
                            *f = true;
                        }
                        continue;
                    }
                    if let Ok(mut p) = partial.lock() {
                        *p = line.clone();
                    }
                    if let Ok(mut l) = last_line.lock() {
                        *l = line;
                    }
                }
            }));
        }
        self.stdin = stdin;
        self.child = Some(child);
        true
    }

    /// 结束当前这一句并取走最终文本。
    ///
    /// 默认**保留**子进程和已加载的模型（这是 SenseVoice 能用的关键）：
    /// 先写句末哨兵让它切句，再等它把最后一行打出来。
    /// 写哨兵失败（管道断 / 子进程已死）才真的销毁，下次按键重建。
    fn stop(&mut self) -> Option<String> {
        self.finish_utterance()
    }

    /// 发送句末哨兵，等最后一行。进程保留。
    fn finish_utterance(&mut self) -> Option<String> {
        // 先记下预览阶段已经识别出的文本，作为句末结果为空时的兜底。
        // （句末 sherpa-serve 会整段重新解码，正常情况下比预览更完整；
        //   但若尾巴是静音导致整段解码出空，至少不要丢掉已经算出来的字。）
        let preview = self.partial.lock().map(|p| p.clone()).unwrap_or_default();
        self.partial.lock().map(|mut p| p.clear()).ok();
        self.last_line.lock().map(|mut l| l.clear()).ok();

        if self.stdin.is_none() {
            // 没进程（空会话），直接返回
            return None;
        }

        let sent = match self.stdin.as_mut() {
            Some(stdin) => {
                stdin.write_all(&SENTINEL).is_ok() && stdin.flush().is_ok()
            }
            None => false,
        };
        if !sent {
            // 管道已断，进程也可能死了：彻底清掉，下次重建
            self.kill();
            return None;
        }

        // 等子进程报告本句结束。
        //
        // 两种子进程都要兼容：
        // - 认识哨兵的（sherpa-serve）→ 输出最终行 + 空行，进程继续存活；
        // - 不认识哨兵的旧 wrapper → 哨兵被当普通音频，只能靠关 stdin 才收尾。
        //   那种情况在超时/无新输出后关掉 stdin 兵底（进程会退出，下次重建，
        //   仅影响旧 wrapper，常驻路径不受影响）。
        let mut last = String::new();
        let mut stable = 0;
        let start = std::time::Instant::now();
        let deadline = start + std::time::Duration::from_secs(15);
        // 给认识哨兵的子进程留的宽限；过了这段还没动静就当它不认哨兵
        let sentinel_grace = start + std::time::Duration::from_millis(1500);
        let mut stdin_closed = false;
        while std::time::Instant::now() < deadline {
            // 空行信号：sherpa-serve 明确告知本句已完成
            if self.finished.lock().map(|f| *f).unwrap_or(false) {
                break;
            }
            let current = self.last_line.lock().map(|l| l.clone()).unwrap_or_default();
            if !current.is_empty() {
                if current == last {
                    stable += 1;
                    // 最后一行稳定下来（~300ms 没变）就认为写完了。
                    // 但如果已经关了 stdin，仍以「读线程结束」为准确信号，
                    // 否则会在真 final 到达前提前退出（旧 wrapper 靠 EOF 后才打 final）。
                    if stable >= 15 && !stdin_closed {
                        break;
                    }
                } else {
                    last = current;
                    stable = 0;
                }
            }
            // 宽限期过了还没收到结束信号：关掉 stdin 让它收尾（兼容旧 wrapper）
            if !stdin_closed && std::time::Instant::now() >= sentinel_grace {
                self.stdin.take();
                stdin_closed = true;
                // 关掉后它应该收尾退出；重置稳定计数，再给一段等待时间
                stable = 0;
                continue;
            }
            // 已经关了 stdin，且读线程已结束（子进程退出、stdout 到底）→ 可以取了
            if stdin_closed {
                if let Some(r) = self.reader.as_ref() {
                    if r.is_finished() {
                        break;
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        let text = self.last_line.lock().map(|l| l.clone()).unwrap_or_default();
        self.partial.lock().map(|mut p| p.clear()).ok();
        self.last_line.lock().map(|mut l| l.clear()).ok();
        self.finished.lock().map(|mut f| *f = false).ok();
        // 句末结果为空时退回预览文本（见上面 preview 的注释）
        let text = if text.is_empty() { preview } else { text };
        if text.is_empty() {
            None
        } else {
            Some(crate::t2s::to_simplified(&text))
        }
    }

    /// 彻底关掉子进程（关 stdin + 等退出 + 必要时 kill）
    fn kill(&mut self) {
        self.stdin.take();
        if let Some(mut child) = self.child.take() {
            // 不无限等：某些 wrapper 不会自己退，最多等 3 秒就 kill，
            // 否则会把 fcitx5 主线程卡死。
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if std::time::Instant::now() < deadline => {
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                    _ => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                }
            }
        }
        // 等读线程自己退出（stdout 已关会很快）。同样不无限等。
        if let Some(reader) = self.reader.take() {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while !reader.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            if reader.is_finished() {
                let _ = reader.join();
            }
        }
        self.partial.lock().map(|mut p| p.clear()).ok();
        self.last_line.lock().map(|mut l| l.clear()).ok();
        self.finished.lock().map(|mut f| *f = false).ok();
    }
}

impl AsrChannel for StreamingChannel {
    fn name(&self) -> &str {
        &self.name
    }

    fn feed(&mut self, pcm: &[i16]) {
        if pcm.is_empty() || !self.ensure_started() {
            return;
        }
        let mut bytes = Vec::with_capacity(pcm.len() * 2);
        for s in pcm {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        // 只在这里借一次 stdin；写失败就丢掉句柄（下次会话重来，不 panic）
        let failed = match self.stdin.as_mut() {
            Some(stdin) => {
                let wrote = stdin.write_all(&bytes).is_ok();
                let flushed = stdin.flush().is_ok();
                !(wrote && flushed)
            }
            None => true,
        };
        if failed {
            self.stdin = None;
        }
    }

    /// 中间结果存在 `Arc<Mutex<String>>` 里，`&self` 借不出来，走 owned 版本。
    fn partial_owned(&self) -> Option<String> {
        let text = self.partial.lock().ok()?.clone();
        if text.is_empty() {
            None
        } else {
            Some(text)
        }
    }

    fn finish(&mut self) -> Option<String> {
        self.stop()
    }

    fn reset(&mut self) {
        // 只清内部状态，**不发哨兵、不杀进程**（模型已在内存，下次按键复用）。
        // 发哨兵是 finish_utterance 的事；这里再发一次会让子进程在空音频上
        // 多输出一个空行，那行会污染下一句（下一句一开头就被误判为「已结束」）。
        self.partial.lock().map(|mut p| p.clear()).ok();
        self.last_line.lock().map(|mut l| l.clear()).ok();
        self.finished.lock().map(|mut f| *f = false).ok();
    }
}

impl Drop for StreamingChannel {
    /// 渠道被丢弃（插件卸载 / 后端配置变更）时把子进程收掉，不留殏尸。
    fn drop(&mut self) {
        self.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_without_prefix_is_process() {
        assert_eq!(
            Backend::parse("whisper-cli -m /x.bin -l zh"),
            Backend::Process("whisper-cli -m /x.bin -l zh".to_string())
        );
    }

    #[test]
    fn parse_sherpa_prefixes() {
        let b = Backend::parse("sherpa-streaming:sherpa-onnx --tokens=/t.txt --encoder=/e.onnx");
        match b {
            Backend::Sherpa(opts) => {
                assert_eq!(opts.command[0], "sherpa-onnx");
                assert_eq!(opts.command[1], "--tokens=/t.txt");
            }
            other => panic!("期望 Sherpa，实际 {other:?}"),
        }
        assert!(matches!(
            Backend::parse("sherpa-offline:sherpa-onnx-offline --x"),
            Backend::Sherpa(_)
        ));
        assert!(matches!(
            Backend::parse("sherpa:sherpa-onnx-offline --x"),
            Backend::Sherpa(_)
        ));
    }

    #[test]
    fn parse_stream_prefix() {
        assert_eq!(
            Backend::parse("stream:my-asr --lang zh"),
            Backend::Streaming("my-asr --lang zh".to_string())
        );
    }

    #[test]
    fn empty_command_builds_no_channel() {
        assert!(Backend::parse("").build_channel().is_none());
        assert!(Backend::parse("sherpa-streaming:").build_channel().is_none());
        assert!(Backend::parse("stream:   ").build_channel().is_none());
    }

    #[test]
    fn sherpa_options_parse() {
        let opts = SherpaOptions::parse("--tokens=/t.txt --paraformer=/p.onnx");
        // 用户没写程序名时 program() 自动补 sherpa-onnx-offline
        assert_eq!(opts.command[0], "--tokens=/t.txt");
        assert_eq!(
            opts.program(),
            vec![
                "sherpa-onnx-offline",
                "--tokens=/t.txt",
                "--paraformer=/p.onnx"
            ]
        );
        // 写了程序名就不再动
        let opts = SherpaOptions::parse("sherpa-onnx --tokens=/t.txt");
        assert_eq!(opts.program(), opts.command);
        // 空串：program() 只给默认程序
        assert!(SherpaOptions::parse("").command.is_empty());
        assert_eq!(
            SherpaOptions::parse("").program(),
            vec!["sherpa-onnx-offline".to_string()]
        );
    }

    #[test]
    fn sherpa_options_drop_wav() {
        let opts = SherpaOptions::parse(
            "sherpa-onnx-offline --tokens=/t.txt --wav=/tmp/x.wav --paraformer=/p.onnx",
        );
        assert_eq!(
            opts.command_without_wav(),
            vec![
                "sherpa-onnx-offline",
                "--tokens=/t.txt",
                "--paraformer=/p.onnx"
            ]
        );
        let opts = SherpaOptions::parse("sherpa-onnx-offline --wav /tmp/x.wav --tokens=/t");
        assert_eq!(
            opts.command_without_wav(),
            vec!["sherpa-onnx-offline", "--tokens=/t"]
        );
    }

    #[test]
    fn sherpa_resolved_command() {
        // 用户只写选项，也要能跑起来（补 sherpa-onnx-offline）
        let opts = SherpaOptions::parse("--tokens=/t.txt --paraformer=/p.onnx --wav=/x.wav");
        assert_eq!(
            opts.resolved_command(),
            vec![
                "sherpa-onnx-offline",
                "--tokens=/t.txt",
                "--paraformer=/p.onnx"
            ]
        );
        // 写了程序名就保留
        let opts = SherpaOptions::parse("sherpa-onnx --tokens=/t.txt");
        assert_eq!(
            opts.resolved_command(),
            vec!["sherpa-onnx", "--tokens=/t.txt"]
        );
    }

    #[test]
    fn min_audio_threshold_is_300ms() {
        // 16000Hz * 0.3s
        assert_eq!(MIN_SAMPLES, 4800);
        assert_eq!(MIN_AUDIO_MS, 300);
    }

    #[test]
    fn sherpa_channel_skips_tiny_clips() {
        // 低于 MIN_SAMPLES 的片段直接返回 None（不会起进程）
        let mut ch = SherpaChannel::new("sherpa", SherpaOptions::parse("--tokens=/t.txt"));
        ch.feed(&vec![0i16; MIN_SAMPLES - 1]);
        assert_eq!(ch.finish(), None);

        // 空输入同样为 None
        let mut empty = SherpaChannel::new("sherpa", SherpaOptions::parse("--tokens=/t.txt"));
        assert_eq!(empty.finish(), None);
    }

    #[test]
    fn pick_text_skips_noise() {
        let stderr = "Number of threads: 2, Elapsed seconds: 0.31, Audio duration (s): 3.2, Real time factor (RTF) = 0.1/3.2 = 0.03\n\
                      今天天气不错\n\
                      {\"text\": \"今天天气不错\", \"tokens\": []}\n";
        assert_eq!(pick_text("", stderr).as_deref(), Some("今天天气不错"));
    }

    #[test]
    fn pick_text_falls_back_to_stdout() {
        assert_eq!(
            pick_text("Start to create recognizer\n你好\n", "").as_deref(),
            Some("你好")
        );
        assert_eq!(pick_text("", ""), None);
    }

    // 用 python 模拟「stdin 流式」子进程：每读到一段就吐一行，验证 partial/final 链路。
    // 用 os.read 而不是 buffer.read：后者会等满请求字节数，测试里容易卡住。
    #[test]
    fn streaming_channel_reads_partial_from_child() {
        // 用唯一文件名：测试并行跑，同名会被彼此覆盖/删除
        let script = std::env::temp_dir().join(format!(
            "fcitx5_voice_fake_stream_{}_{}.py",
            std::process::id(),
            line!()
        ));
        // 用 concat! 逐行拼，避免 Rust 的 `\` 续行吃掉 Python 的缩进
        let py = concat!(
            "import os, sys\n",
            "n = 0\n",
            "while True:\n",
            "    b = os.read(0, 4096)\n",
            "    if not b:\n",
            "        break\n",
            "    n += 1\n",
            "    sys.stdout.write('part%d\\n' % n)\n",
            "    sys.stdout.flush()\n",
            "sys.stdout.write('final\\n')\n",
            "sys.stdout.flush()\n",
        );
        std::fs::write(&script, py).unwrap();
        let cmd = format!("python3 {}", script.display());
        let mut ch = StreamingChannel::new("t", &cmd);
        assert!(ch.partial_owned().is_none());
        // 轮询等待（python 起进程 + 首轮读取需要几十到几百毫秒）
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while ch.partial_owned().is_none() && std::time::Instant::now() < deadline {
            ch.feed(&[0i16; 2048]);
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(
            ch.partial_owned().is_some(),
            "子进程应已吐出至少一行中间结果"
        );
        // 这个假后端**不认哨兵**，只会在 EOF 后才吐 final；新协议下会在宽限期
        // 后关 stdin 兜底，所以拿到的可能是它最后吐的 part* 或 final——两者都算
        // 合法收尾（真实常驻后端走哨兵路径，由 sentinel_separates_utterances 覆盖）。
        let done = ch.finish();
        assert!(done.is_some(), "旧协议后端也应能收回结果，而不是 None：{done:?}");
        assert!(ch.partial_owned().is_none());
        let _ = std::fs::remove_file(&script);
    }

    #[test]
    fn streaming_channel_sentinel_separates_utterances() {
        // 进程常驻 + 句末哨兵：每收到哨兵就输出并继续等，不退出
        let script = std::env::temp_dir().join(format!(
            "fcitx5_voice_fake_sentinel_{}_{}.py",
            std::process::id(),
            line!()
        ));
        // 假后端严格按协议实现：逐字节读（os.read 而不是缓冲 read），
        // 前缀不明时等待而不是提前消费 2 字节，否则哨兵会被当成音频吃掉。
        // 句末输出一行结果 + 一个空行（告知「本句结束」）。
        let py = concat!(
            "import sys, os\n",
            "buf = b''\n",
            "total = 0\n",
            "while True:\n",
            "    b = os.read(0, 1)\n",
            "    if not b:\n",
            "        break\n",
            "    buf += b\n",
            "    while True:\n",
            "        if not buf:\n",
            "            break\n",
            "        if buf[0] != 0xFF:\n",
            "            if len(buf) >= 2:\n",
            "                buf = buf[2:]\n",
            "                total += 1\n",
            "                continue\n",
            "            break\n",
            "        if len(buf) >= 4:\n",
            "            if buf[:4] == b'\\xff\\xff\\xff\\xff':\n",
            "                buf = buf[4:]\n",
            "                sys.stdout.write('utt%d\\n' % total)\n",
            "                sys.stdout.write('\\n')\n",
            "                sys.stdout.flush()\n",
            "                total = 0\n",
            "                continue\n",
            "            buf = buf[2:]\n",
            "            total += 1\n",
            "            continue\n",
            "        break\n",
        );
        std::fs::write(&script, py).unwrap();
        let cmd = format!("python3 -u {}", script.display());
        let mut ch = StreamingChannel::new("t", &cmd);

        // 第一句
        ch.feed(&[1i16; 1000]);
        let first = ch.finish();
        assert_eq!(first.as_deref(), Some("utt1000"), "第一句应有结果（整句 1000 样本）");
        assert!(ch.partial_owned().is_none(), "句后 partial 应清空");

        // 第二句：同一进程复用（如果进程死了，这步会失败或超时）
        ch.feed(&[2i16; 1000]);
        let second = ch.finish();
        assert_eq!(second.as_deref(), Some("utt1000"), "第二句应复用同一进程（计数已复位）");

        let _ = std::fs::remove_file(&script);
    }

    #[test]
    fn streaming_channel_accumulates_whole_utterance() {
        // 回归测试：句末结果必须是**整句**音频，而不是最后一小段。
        // 历史上 sherpa-serve 在中间预览后把缓冲清空，导致句末只解了最后不到 2 秒，
        // 上屏为空。这里用一个回显累计样本数的假后端钉住这个行为。
        let script = std::env::temp_dir().join(format!(
            "fcitx5_voice_fake_accum_{}_{}.py",
            std::process::id(),
            line!()
        ));
        let py = concat!(
            "import sys, os\n",
            "buf = b''\n",
            "total = 0\n",
            "while True:\n",
            "    b = os.read(0, 1)\n",
            "    if not b:\n",
            "        break\n",
            "    buf += b\n",
            "    while True:\n",
            "        if not buf:\n",
            "            break\n",
            "        if buf[0] != 0xFF:\n",
            "            if len(buf) >= 2:\n",
            "                buf = buf[2:]\n",
            "                total += 1\n",
            "                continue\n",
            "            break\n",
            "        if len(buf) >= 4:\n",
            "            if buf[:4] == b'\\xff\\xff\\xff\\xff':\n",
            "                buf = buf[4:]\n",
            "                sys.stdout.write('total%d\\n' % total)\n",
            "                sys.stdout.flush()\n",
            "                total = 0\n",
            "                continue\n",
            "            buf = buf[2:]\n",
            "            total += 1\n",
            "            continue\n",
            "        break\n",
        );
        std::fs::write(&script, py).unwrap();
        let cmd = format!("python3 -u {}", script.display());
        let mut ch = StreamingChannel::new("t", &cmd);

        // 分 3 次嗂 3000 个样本，句末应报告 3000（整句），而不是最后一块
        ch.feed(&[1i16; 1000]);
        ch.feed(&[1i16; 1000]);
        ch.feed(&[1i16; 1000]);
        assert_eq!(
            ch.finish().as_deref(),
            Some("total3000"),
            "句末必须用整句音频，不能只解最后一段"
        );

        // 第二句从零开始计数
        ch.feed(&[2i16; 500]);
        assert_eq!(ch.finish().as_deref(), Some("total500"));

        let _ = std::fs::remove_file(&script);
    }

    #[test]
    fn streaming_channel_missing_program_is_safe() {
        let mut ch = StreamingChannel::new("t", "/nonexistent/asr-program-xyz");
        ch.feed(&[1i16, 2, 3]);
        assert!(ch.finish().is_none());
    }

    /// 假流式程序：每读到一段二进制就回一行固定文本，验证 partial 能拿到最新一行。
    /// 注意不能用 `sh -c 'while read -r l'`——PCM 是二进制、没有换行，会永远不返回。
    #[test]
    fn streaming_partial_updates_from_lines() {
        let mut ch = StreamingChannel::new(
            "t",
            "python3 -c \"import os,sys\nwhile True:\n    b=os.read(0,4096)\n    if not b: break\n    sys.stdout.write('got\\n');sys.stdout.flush()\"",
        );
        ch.feed(&[0i16; 2048]);
        std::thread::sleep(std::time::Duration::from_millis(400));
        assert_eq!(ch.partial_owned().as_deref(), Some("got"));
        assert_eq!(ch.finish().as_deref(), Some("got"));
    }
}
