//! 本地语音输入核心（与平台解耦）。
//!
//! 链路：热键状态机 -> 采集 -> 重采样/分帧 -> VAD -> ASR 渠道 -> 上屏差分
//!
//! 平台相关部分（fcitx5 事件拦截、麦克风、`commitString` 上屏）放在 C++ 插件里，
//! 由 `src/ffi.rs` 暴露成 C ABI；这样核心逻辑能在任何系统上单测。

pub mod asr;
pub mod audio;
#[cfg(feature = "capture")]
pub mod capture;
pub mod ffi;
pub mod hotkey;
pub mod sherpa;
pub mod t2s;
pub mod typer;
pub mod vad;
pub mod wav;

pub use asr::{AsrChannel, MultiChannel, ProcessChannel, Transcript};
pub use audio::{CaptureFormat, Framer, Resampler, FRAME_SAMPLES, TARGET_RATE};
pub use hotkey::{Action, Mode, TriggerConfig, VoiceTrigger};
pub use sherpa::{Backend, SherpaChannel, SherpaOptions, StreamingChannel};
pub use t2s::to_simplified;
pub use typer::{Typer, Update};
pub use vad::Vad;
pub use wav::{read_wav, write_wav};

use std::time::Instant;

/// 一次语音会话的编排器：把热键动作、音频、识别、上屏串起来。
///
/// 与 WowType 的实现一致（同上游 voicekey 的语义）：
/// - 按下即录音；<300ms 松开判点按（继续录到静音或再按一次），>=300ms 判长按（立即停）
/// - 说话时 `push_pcm` 推进，`partial` 返回边说边出的中间结果
/// - `finish` 结束并返回最终文本
pub struct VoiceSession {
    trigger: VoiceTrigger,
    framer: Framer,
    vad: Vad,
    asr: MultiChannel,
    typer: Typer,
    last_partial: String,
}

impl VoiceSession {
    pub fn new(cfg: TriggerConfig, asr: MultiChannel) -> Self {
        Self {
            trigger: VoiceTrigger::new(cfg),
            framer: Framer::default(),
            vad: Vad::default(),
            asr,
            typer: Typer::new(),
            last_partial: String::new(),
        }
    }

    /// 键按下：返回状态机动作
    pub fn on_key_down(&mut self, now: Instant) -> Vec<Action> {
        self.trigger.on_down(now)
    }

    /// 键松开：返回状态机动作
    pub fn on_key_up(&mut self, now: Instant) -> Vec<Action> {
        self.trigger.on_up(now)
    }

    /// 周期驱动：点按模式静音超时会返回 Stop
    pub fn tick(&mut self, now: Instant) -> Option<Action> {
        self.trigger.tick(now)
    }

    pub fn is_recording(&self) -> bool {
        self.trigger.is_recording()
    }

    pub fn mode(&self) -> Mode {
        self.trigger.mode()
    }

    /// 推入采集到的 16kHz 单声道 PCM，返回「边说边出字」的增量（没有变化时为 None）。
    pub fn push_pcm(&mut self, pcm: &[i16], now: Instant) -> Option<Update> {
        if !self.trigger.is_recording() {
            return None;
        }
        for frame in self.framer.push(pcm) {
            if self.vad.push(&frame) {
                self.trigger.note_voice(now);
            }
            self.asr.feed(&frame);
        }
        let partial = self
            .asr
            .partials()
            .first()
            .map(|(_, t)| t.clone())
            .unwrap_or_default();
        if partial != self.last_partial {
            let update = self.typer.update(&partial);
            self.last_partial = partial;
            return Some(update);
        }
        None
    }

    /// 结束会话，返回 (最终文本, 需要补的增量)。
    ///
    /// 第二个返回值是「把已经上屏的中间结果修正成最终文本」所需的差分；
    /// 调用方按 `backspaces` 退格、再插入 `insert` 即可。
    pub fn finish(&mut self) -> (String, Update) {
        if let Some(tail) = self.framer.flush() {
            self.asr.feed(&tail);
        }
        let results = self.asr.finish();
        let text = results
            .first()
            .map(|t| t.text.clone())
            .unwrap_or_else(|| self.last_partial.clone());
        let update = self.typer.update(&text);
        self.typer.finish();
        self.last_partial.clear();
        // 会话结束：状态机必须回到空闲，否则下一次按下会被当成「已经在录」而忽略
        self.trigger.reset();
        self.framer.reset();
        self.asr.reset();
        (text, update)
    }

    /// 当前中间结果的字符数（界面上做音量/进度指示用）
    pub fn partial_len(&self) -> usize {
        self.last_partial.chars().count()
    }

    /// 丢弃当前会话（按 Esc / 打字 / 失焦）
    pub fn cancel(&mut self) {
        self.trigger.reset();
        self.framer.reset();
        self.asr.reset();
        self.typer.reset();
        self.last_partial.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn session_runs_end_to_end() {
        let t0 = Instant::now();
        let mut s = VoiceSession::new(
            TriggerConfig::default(),
            MultiChannel::new(vec![Box::new(asr::MockChannel::new("mock"))]),
        );
        s.on_key_down(t0);
        assert!(s.is_recording());
        s.push_pcm(&vec![0i16; 1600], t0 + Duration::from_millis(200));
        let (text, _update) = s.finish();
        assert_eq!(text, "mock:1600");
        assert!(!s.is_recording());
    }

    #[test]
    fn hold_release_stops() {
        let t0 = Instant::now();
        let mut s = VoiceSession::new(
            TriggerConfig::default(),
            MultiChannel::new(vec![Box::new(asr::MockChannel::new("m"))]),
        );
        s.on_key_down(t0);
        s.on_key_up(t0 + Duration::from_millis(500));
        assert!(!s.is_recording());
    }

    #[test]
    fn tap_keeps_recording_until_silence() {
        let t0 = Instant::now();
        let mut s = VoiceSession::new(
            TriggerConfig::default(),
            MultiChannel::new(vec![Box::new(asr::MockChannel::new("m"))]),
        );
        s.on_key_down(t0);
        s.on_key_up(t0 + Duration::from_millis(100));
        assert!(s.is_recording());
        assert_eq!(s.tick(t0 + Duration::from_millis(1000)), None);
        assert_eq!(s.tick(t0 + Duration::from_millis(2000)), Some(Action::Stop));
    }

    #[test]
    fn cancel_drops_recording() {
        let t0 = Instant::now();
        let mut s = VoiceSession::new(
            TriggerConfig::default(),
            MultiChannel::new(vec![Box::new(asr::MockChannel::new("m"))]),
        );
        s.on_key_down(t0);
        s.cancel();
        assert!(!s.is_recording());
    }
}
