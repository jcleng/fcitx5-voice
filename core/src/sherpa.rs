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
        if self.pcm.is_empty() {
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
/// 关闭 stdin 表示说完，进程退出前 stdout 最后一行即最终结果。
///
/// 这样不用 unsafe FFI 也能接真流式引擎：写个小脚本包一层 sherpa-onnx C API 即可。
pub struct StreamingChannel {
    name: String,
    program: Vec<String>,
    /// 子进程（会话开始时惰性启动）
    child: Option<Child>,
    /// 往子进程 stdin 写 PCM 的句柄
    stdin: Option<ChildStdin>,
    /// 读线程刷新的「当前中间结果」
    partial: Arc<Mutex<String>>,
    /// 收集读线程看到的最后一行（final 用）
    last_line: Arc<Mutex<String>>,
    /// 读线程句柄（`stop` 时等它读完最后一行）
    reader: Option<std::thread::JoinHandle<()>>,
}

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
        if let Some(stdout) = child.stdout.take() {
            // 读线程只碰两个 Arc<Mutex<String>>，天然 Send
            self.reader = Some(std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    let Ok(line) = line else { break };
                    let line = line.trim().to_string();
                    if line.is_empty() {
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

    /// 结束子进程并取走最终文本
    fn stop(&mut self) -> Option<String> {
        // 关 stdin = 告诉它「说完了」
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
        // 等读线程把最后一行写进 last_line，否则会读早。
        // 不能裸 join：有些 wrapper 不主动关 stdout，读线程会一直阻塞在
        // read 上，裸 join 会把 fcitx5 主线程卡死。这里最多等 2 秒，
        // 超时就把线程丢下不管（它是 detached 的，进程退出时自然回收）。
        if let Some(reader) = self.reader.take() {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while !reader.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            if reader.is_finished() {
                let _ = reader.join();
            }
        }
        let text = self.last_line.lock().map(|l| l.clone()).unwrap_or_default();
        self.partial.lock().map(|mut p| p.clear()).ok();
        self.last_line.lock().map(|mut l| l.clear()).ok();
        if text.is_empty() {
            None
        } else {
            Some(crate::t2s::to_simplified(&text))
        }
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
        let _ = self.stop();
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
        assert_eq!(ch.finish().as_deref(), Some("final"));
        assert!(ch.partial_owned().is_none());
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
