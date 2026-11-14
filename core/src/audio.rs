//! 音频格式、重采样、分帧。
//!
//! 引擎侧（与主流语音识别一致）统一使用 16kHz 单声道 16bit，20ms 一帧。

pub const TARGET_RATE: u32 = 16_000;
pub const FRAME_MS: u32 = 20;
pub const FRAME_SAMPLES: usize = (TARGET_RATE as usize * FRAME_MS as usize) / 1000;

/// 采集参数
#[derive(Clone, Copy, Debug)]
pub struct CaptureFormat {
    pub sample_rate: u32,
    pub channels: u16,
}

impl Default for CaptureFormat {
    fn default() -> Self {
        Self {
            sample_rate: 48_000,
            channels: 1,
        }
    }
}

/// 一阶低通（用于降采样前抗混叠）。截止频率以「占输入采样率的比例」给出。
#[derive(Debug)]
struct Lowpass {
    a: f32,
    y: f32,
}

impl Lowpass {
    fn new(cutoff: f32, _order: u32) -> Self {
        // 单极点 IIR：a = exp(-2*pi*fc)
        let a = (-2.0 * std::f32::consts::PI * cutoff.clamp(0.001, 0.499)).exp();
        Self { a, y: 0.0 }
    }

    fn process(&mut self, input: &[f32]) -> Vec<f32> {
        let mut out = Vec::with_capacity(input.len());
        for &x in input {
            self.y = (1.0 - self.a) * x + self.a * self.y;
            out.push(self.y);
        }
        out
    }
}

/// 重采样：降采样时先过一个低通滤波，再线性插值；无外部依赖。
///
/// 不滤波直接插值的话，44.1k/48k 录音里 8kHz 以上的成分（齿音、风扇声）会折叠进
/// 语音频段，识别会变差。
#[derive(Debug)]
pub struct Resampler {
    from: u32,
    to: u32,
    /// 上一个输入样本，用于跨界插值
    prev: f32,
    /// 小数相位，范围 [0,1)
    pos: f32,
    started: bool,
    lowpass: Option<Lowpass>,
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Self {
        assert!(from > 0 && to > 0);
        // 截止频率取目标奈奎斯特频率的 90%
        let lowpass = (from > to).then(|| Lowpass::new(0.45 * to as f32 / from as f32, 1));
        Self {
            from,
            to,
            prev: 0.0,
            pos: 0.0,
            started: false,
            lowpass,
        }
    }

    pub fn is_identity(&self) -> bool {
        self.from == self.to
    }

    /// 输入 f32 采样，输出重采样后的 f32；长度约为 len*to/from
    pub fn process(&mut self, input: &[f32]) -> Vec<f32> {
        if self.is_identity() {
            return input.to_vec();
        }
        let filtered;
        let input = match self.lowpass.as_mut() {
            Some(lp) => {
                filtered = lp.process(input);
                &filtered[..]
            }
            None => input,
        };
        if input.is_empty() {
            return Vec::new();
        }
        let step = self.from as f32 / self.to as f32;
        let mut out = Vec::with_capacity((input.len() as f32 / step) as usize + 2);
        for &x in input {
            if !self.started {
                self.prev = x;
                self.started = true;
                out.push(x);
                continue;
            }
            // 在 [prev, x] 之间按相位插值
            self.pos += 1.0;
            while self.pos >= step {
                self.pos -= step;
                let t = self.pos / step;
                out.push(self.prev + (x - self.prev) * (1.0 - t));
            }
            self.prev = x;
        }
        out
    }
}

/// f32 -> i16
pub fn to_i16(samples: &[f32]) -> Vec<i16> {
    samples
        .iter()
        .map(|&v| (v.clamp(-1.0, 1.0) * 32767.0).round() as i16)
        .collect()
}

/// 多声道降混成单声道（取平均）
pub fn downmix(interleaved: &[f32], channels: u16) -> Vec<f32> {
    let ch = channels.max(1) as usize;
    if ch == 1 {
        return interleaved.to_vec();
    }
    interleaved
        .chunks_exact(ch)
        .map(|f| f.iter().sum::<f32>() / ch as f32)
        .collect()
}

/// 定长分帧：把任意长度的 PCM 切成 20ms 一帧，余数留到下批。
#[derive(Debug, Default)]
pub struct Framer {
    buf: Vec<i16>,
}

impl Framer {
    pub fn reset(&mut self) {
        self.buf.clear();
    }

    /// 推入 PCM，返回凑满的整帧
    pub fn push(&mut self, pcm: &[i16]) -> Vec<Vec<i16>> {
        self.buf.extend_from_slice(pcm);
        let mut frames = Vec::new();
        while self.buf.len() >= FRAME_SAMPLES {
            let frame: Vec<i16> = self.buf.drain(..FRAME_SAMPLES).collect();
            frames.push(frame);
        }
        frames
    }

    /// 收尾：把不足一帧的尾部也吐出去（补齐），没有则为 None
    pub fn flush(&mut self) -> Option<Vec<i16>> {
        if self.buf.is_empty() {
            return None;
        }
        let mut tail = std::mem::take(&mut self.buf);
        tail.resize(FRAME_SAMPLES, 0);
        Some(tail)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framer_splits_evenly() {
        let mut f = Framer::default();
        let frames = f.push(&vec![0i16; FRAME_SAMPLES * 3 + 5]);
        assert_eq!(frames.len(), 3);
        assert!(frames.iter().all(|x| x.len() == FRAME_SAMPLES));
        assert!(f.flush().is_some());
        assert!(f.flush().is_none());
    }

    #[test]
    fn resampler_halves_length() {
        let mut rs = Resampler::new(32_000, 16_000);
        let out = rs.process(&vec![0.1f32; 3200]);
        assert!((out.len() as i32 - 1600).abs() <= 4);
    }

    #[test]
    fn identity_resampler_passthrough() {
        let mut rs = Resampler::new(16_000, 16_000);
        let input = vec![0.5f32; 100];
        assert_eq!(rs.process(&input), input);
    }

    #[test]
    fn downmix_averages_channels() {
        let out = downmix(&[1.0, 0.0, 0.5, 0.5], 2);
        assert_eq!(out, vec![0.5, 0.5]);
    }

    #[test]
    fn to_i16_clamps() {
        assert_eq!(to_i16(&[0.0, 2.0, -2.0]), vec![0, 32767, -32767]);
    }
}
