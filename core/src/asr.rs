//! 识别渠道抽象。
//!
//! 一个渠道可以是云端流式（WebSocket/HTTP）也可以是本地引擎（sherpa-onnx、
//! whisper.cpp、WowType 的 wtlocal 等）。上层只关心：喂音频、取中间结果、取最终结果。

use std::io::Write;

/// 一次识别结果
#[derive(Clone, Debug, PartialEq)]
pub struct Transcript {
    pub channel: String,
    pub text: String,
    pub latency_ms: u64,
    pub is_final: bool,
}

pub trait AsrChannel: Send {
    fn name(&self) -> &str;
    /// 喂入 16kHz 单声道 PCM
    fn feed(&mut self, pcm: &[i16]);
    /// 当前中间结果（边说边出字）
    fn partial(&self) -> Option<&str> {
        None
    }
    /// 结束并取最终结果
    fn finish(&mut self) -> Option<String>;
    /// 丢弃已收音频
    fn reset(&mut self) {}
}

/// 多渠道并跑：任一渠道出最终结果即可参与候选
pub struct MultiChannel {
    channels: Vec<Box<dyn AsrChannel>>,
}

impl MultiChannel {
    pub fn new(channels: Vec<Box<dyn AsrChannel>>) -> Self {
        Self { channels }
    }

    pub fn is_empty(&self) -> bool {
        self.channels.is_empty()
    }

    pub fn feed(&mut self, pcm: &[i16]) {
        for c in &mut self.channels {
            c.feed(pcm);
        }
    }

    pub fn partials(&self) -> Vec<(&str, &str)> {
        self.channels
            .iter()
            .filter_map(|c| c.partial().map(|p| (c.name(), p)))
            .collect()
    }

    pub fn finish(&mut self) -> Vec<Transcript> {
        let mut out = Vec::new();
        for c in &mut self.channels {
            if let Some(text) = c.finish() {
                if !text.is_empty() {
                    out.push(Transcript {
                        channel: c.name().to_string(),
                        text,
                        latency_ms: 0,
                        is_final: true,
                    });
                }
            }
        }
        out
    }

    pub fn reset(&mut self) {
        for c in &mut self.channels {
            c.reset();
        }
    }
}

/// 测试/占位用渠道：把收到的音频长度当作「识别结果」
#[derive(Debug, Default)]
pub struct MockChannel {
    name: String,
    samples: usize,
}

impl MockChannel {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            samples: 0,
        }
    }
}

impl AsrChannel for MockChannel {
    fn name(&self) -> &str {
        &self.name
    }
    fn feed(&mut self, pcm: &[i16]) {
        self.samples += pcm.len();
    }
    fn finish(&mut self) -> Option<String> {
        Some(format!("{}:{}", self.name, self.samples))
    }
    fn reset(&mut self) {
        self.samples = 0;
    }
}

/// 外部命令式识别渠道：把整段音频写成 WAV，交给命令行识别器，读 stdout。
///
/// 这样不用编译任何原生绑定就能接 whisper.cpp / sherpa-onnx：
///
/// ```text
/// whisper-cli -m ggml-large-v3-turbo.bin -l zh -nt -f
/// sherpa-onnx-offline --tokens=... --paraformer=... --wav=
/// ```
///
/// 命令末尾会自动追加 WAV 路径。识别只在会话结束时跑一次，因此没有中间结果
/// （要边说边出字请用流式引擎，或在本渠道之上另接增量引擎）。
pub struct ProcessChannel {
    name: String,
    program: Vec<String>,
    pcm: Vec<i16>,
}

impl ProcessChannel {
    pub fn new(name: impl Into<String>, cmdline: &str) -> Self {
        Self {
            name: name.into(),
            program: split_command(cmdline),
            pcm: Vec::new(),
        }
    }
}

/// 把命令行拆成参数，支持单/双引号和反斜杠转义。
///
/// 不能直接用 split_whitespace：模型路径里常有空格（如
/// "/home/me/My Models/ggml.bin"），按空格切会把它拆坏。
fn split_command(cmdline: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut has_token = false;
    let mut chars = cmdline.chars().peekable();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), ch) if ch == q => quote = None,
            (Some(_), ch) => cur.push(ch),
            (None, '"') | (None, '\'') => {
                quote = Some(c);
                has_token = true;
            }
            (None, '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                    has_token = true;
                }
            }
            (None, ch) if ch.is_whitespace() => {
                if has_token || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    has_token = false;
                }
            }
            (None, ch) => {
                cur.push(ch);
                has_token = true;
            }
        }
    }
    if has_token || !cur.is_empty() {
        out.push(cur);
    }
    out
}

impl AsrChannel for ProcessChannel {
    fn name(&self) -> &str {
        &self.name
    }

    fn feed(&mut self, pcm: &[i16]) {
        self.pcm.extend_from_slice(pcm);
    }

    fn finish(&mut self) -> Option<String> {
        if self.pcm.is_empty() {
            return None;
        }
        let path = std::env::temp_dir().join(format!("fcitx5-voice-{}.wav", std::process::id()));
        if crate::wav::write_wav(&path, &self.pcm).is_err() {
            self.pcm.clear();
            return None;
        }
        let (bin, args) = self.program.split_first()?;
        let out = std::process::Command::new(bin)
            .args(args)
            .arg(&path)
            .output()
            .ok();
        let _ = std::fs::remove_file(&path);
        self.pcm.clear();
        let out = out?;
        if !out.status.success() {
            return None;
        }
        let mut text = String::from_utf8_lossy(&out.stdout).to_string();
        if text.is_empty() {
            // 有些识别器把结果写到 stderr
            text = String::from_utf8_lossy(&out.stderr).to_string();
        }
        let text = text.trim().to_string();
        if text.is_empty() {
            None
        } else {
            Some(text)
        }
    }

    fn reset(&mut self) {
        self.pcm.clear();
    }
}

/// 让编译器别把 `Write` 当未用（`wav` 模块用了它，这里保持 import 稳定）。
#[allow(dead_code)]
fn _assert_write_bound<T: Write>() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_handles_quotes_and_spaces() {
        assert_eq!(
            split_command("whisper-cli -m \"/home/me/My Models/ggml.bin\" -l zh"),
            vec!["whisper-cli", "-m", "/home/me/My Models/ggml.bin", "-l", "zh"]
        );
    }

    #[test]
    fn split_handles_single_quotes() {
        assert_eq!(split_command("a 'b c' d"), vec!["a", "b c", "d"]);
    }

    #[test]
    fn multichannel_collects_results() {
        let mut mc = MultiChannel::new(vec![
            Box::new(MockChannel::new("a")),
            Box::new(MockChannel::new("b")),
        ]);
        mc.feed(&[0i16; 100]);
        let out = mc.finish();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].text, "a:100");
    }

    #[test]
    fn mock_reset_clears() {
        let mut ch = MockChannel::new("x");
        ch.feed(&[0i16; 50]);
        ch.reset();
        assert_eq!(ch.finish().unwrap(), "x:0");
    }
}
