//! C ABI：给 fcitx5 插件（C++）调用的稳定接口。
//!
//! 所有函数都不阻塞；`poll` 返回状态码，`text` 取走文本，
//! 调用方按 50ms 周期轮询即可（与 fcitx5 的事件循环契合）。
//!
//! 线程模型：
//! - 每次说话起一个录音线程（`capture` 特性打开时用 cpal，否则不录音，
//!   仅靠外部 WAV 回放做测试）
//! - 识别在同一个录音线程里跑，主线程只做 `poll`
//! - `VoiceHandle` 内部用 `Mutex` 保护会话状态

use std::ffi::{c_char, c_int, CStr, CString};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::asr::{AsrChannel, MultiChannel, ProcessChannel};
use crate::hotkey::{Action, Mode, TriggerConfig};
use crate::typer::Update;
use crate::VoiceSession;

/// 语音状态
pub const VOICE_IDLE: c_int = 0;
pub const VOICE_LISTENING: c_int = 1;
pub const VOICE_RECOGNIZING: c_int = 2;
pub const VOICE_DONE: c_int = 3;
pub const VOICE_ERROR: c_int = 4;

/// 后端状态
pub const MODEL_MISSING: c_int = 1;

struct Inner {
    session: VoiceSession,
    phase: c_int,
    /// 需要吐给插件的文本增量
    pending: Option<Update>,
    /// 最终文本（DONE 后可取）
    final_text: String,
    /// 错误信息
    error: String,
    /// 当前音量 0-100（界面上画波形用）
    level: c_int,
    /// 识别后端命令行（空 = 没有后端）
    backend_cmd: String,
    /// 采到的音量（采集回调写，主线程读）
    audio_level: Arc<Mutex<f32>>,
    /// 采集回调写入的 PCM 缓冲（主线程 tick 时取走喂给会话）
    capture_pcm: Arc<Mutex<Vec<i16>>>,
    /// 停止采集的信号（回调看到后不再收数据）
    stop_capture: Arc<std::sync::atomic::AtomicBool>,
    /// 活着的采集流（drop 即关闭麦克风）；cpal::Stream 是 !Send，只能留在本线程
    #[cfg(feature = "capture")]
    capture: Option<crate::capture::Capture>,
}

fn build_channels(cmd: &str) -> Vec<Box<dyn AsrChannel>> {
    let mut channels: Vec<Box<dyn AsrChannel>> = Vec::new();
    if !cmd.trim().is_empty() {
        channels.push(Box::new(ProcessChannel::new("process", cmd)));
    }
    channels
}

impl Inner {
    fn new(cfg: TriggerConfig, backend_cmd: String) -> Self {
        let session = VoiceSession::new(cfg, MultiChannel::new(build_channels(&backend_cmd)));
        Self {
            session,
            phase: VOICE_IDLE,
            pending: None,
            final_text: String::new(),
            error: String::new(),
            level: 0,
            backend_cmd,
            audio_level: Arc::new(Mutex::new(0.0)),
            capture_pcm: Arc::new(Mutex::new(Vec::new())),
            stop_capture: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            #[cfg(feature = "capture")]
            capture: None,
        }
    }

    fn has_backend(&self) -> bool {
        !self.backend_cmd.trim().is_empty()
    }

    /// 会话已结束/未开始，重建一个干净的（渠道随后端变化）
    fn reset_session(&mut self, cfg: TriggerConfig) {
        self.session = VoiceSession::new(cfg, MultiChannel::new(build_channels(&self.backend_cmd)));
    }
}

/// 启动麦克风采集线程。松开时由 `stop_capture` 置位让它退出。
///
/// 采集线程每拿到一批 16k 单声道 PCM，就加锁把 PCM 喂给会话（同时用它更新
/// 音量）；识别仍然在松开时的 `finish` 里跑（ProcessChannel 整段喂命令行）。
#[cfg(feature = "capture")]
fn start_capture(inner: Arc<Mutex<Inner>>) -> Result<(), String> {
    use std::sync::atomic::Ordering;

    // 回调线程只碰这两个（都是 Send）：PCM 缓冲 + 音量。
    // 不捕获 Inner——Inner 里有 cpal::Stream（!Send），不能进回调。
    let pcm_buf: Arc<Mutex<Vec<i16>>> = Arc::new(Mutex::new(Vec::new()));
    let level = Arc::new(Mutex::new(0.0f32));
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

    {
        let mut g = inner.lock().unwrap();
        g.capture_pcm = pcm_buf.clone();
        g.audio_level = level.clone();
        g.stop_capture = stop.clone();
    }

    // 测试/无麦克风环境：跳过真实采集（用 voice_push_pcm 手工喂）
    if std::env::var_os("FCITX5_VOICE_TEST_NO_CAPTURE").is_some() {
        inner.lock().unwrap().capture = None;
        return Ok(());
    }

    let cb_buf = pcm_buf.clone();
    let cb_level = level.clone();
    let cb_stop = stop.clone();
    let capture = crate::capture::start(None, move |pcm: Vec<i16>| {
        if cb_stop.load(Ordering::SeqCst) {
            return;
        }
        if !pcm.is_empty() {
            let sum: f64 = pcm.iter().map(|&s| (s as f64 / 32768.0).powi(2)).sum();
            let rms = (sum / pcm.len() as f64).sqrt() as f32;
            if let Ok(mut l) = cb_level.lock() {
                *l = (rms * 6.0).clamp(0.0, 1.0);
            }
            if let Ok(mut b) = cb_buf.lock() {
                b.extend_from_slice(&pcm);
            }
        }
    })
    .map_err(|e| format!("打开麦克风失败: {e}"))?;

    // cpal::Stream 是 !Send，留在本线程，跟句柄一起活
    inner.lock().unwrap().capture = Some(capture);
    Ok(())
}

#[cfg(not(feature = "capture"))]
fn start_capture(_inner: Arc<Mutex<Inner>>) -> Result<(), String> {
    Err("这个构建没有编译麦克风采集（需要 --features capture）".to_string())
}

/// 插件持有的句柄（不透明指针）。
pub struct VoiceHandle {
    inner: Arc<Mutex<Inner>>,
    cfg: TriggerConfig,
}

/// 读一个 C 字符串，NULL 或非 UTF-8 时返回 fallback
unsafe fn cstr_or(s: *const c_char, fallback: &str) -> String {
    if s.is_null() {
        return fallback.to_string();
    }
    unsafe { CStr::from_ptr(s) }
        .to_str()
        .map(|v| v.to_string())
        .unwrap_or_else(|_| fallback.to_string())
}

/// 创建句柄。
///
/// - `backend_cmd`：识别命令行，末尾会自动追加 WAV 路径；空串表示没有后端
/// - `hold_ms`：长短按分界（毫秒）
/// - `silence_ms`：点按模式的静音超时（毫秒）
///
/// 返回的指针用 [`voice_free`] 释放。
#[no_mangle]
pub unsafe extern "C" fn voice_new(
    backend_cmd: *const c_char,
    hold_ms: u32,
    silence_ms: u32,
) -> *mut VoiceHandle {
    let mut cfg = TriggerConfig::default();
    if hold_ms > 0 {
        cfg.hold_threshold = std::time::Duration::from_millis(hold_ms as u64);
    }
    if silence_ms > 0 {
        cfg.silence_timeout = std::time::Duration::from_millis(silence_ms as u64);
    }
    let cmd = unsafe { cstr_or(backend_cmd, "") };
    let inner = Arc::new(Mutex::new(Inner::new(cfg, cmd)));
    Box::into_raw(Box::new(VoiceHandle { inner, cfg }))
}

/// 释放句柄
#[no_mangle]
pub unsafe extern "C" fn voice_free(handle: *mut VoiceHandle) {
    if handle.is_null() {
        return;
    }
    drop(unsafe { Box::from_raw(handle) });
}

/// 有没有配置识别后端（0=没有，1=有）。没有后端时不该开麦克风。
#[no_mangle]
pub unsafe extern "C" fn voice_ready(handle: *mut VoiceHandle) -> c_int {
    if handle.is_null() {
        return 0;
    }
    let inner = unsafe { &*handle };
    let guard = inner.inner.lock().unwrap();
    guard.has_backend() as c_int
}

/// 键按下。返回 1 表示开始了一个新会话，0 表示忽略。
#[no_mangle]
pub unsafe extern "C" fn voice_key_down(handle: *mut VoiceHandle) -> c_int {
    if handle.is_null() {
        return 0;
    }
    let inner = unsafe { &*handle };
    let now = Instant::now();
    let mut guard = inner.inner.lock().unwrap();
    if !guard.has_backend() {
        guard.phase = VOICE_ERROR;
        guard.error = "没有配置语音识别后端".to_string();
        return 0;
    }
    if !guard.session.is_recording() {
        // 上一轮结束/取消后，起一个干净会话
        guard.reset_session(inner.cfg);
    }
    let actions = guard.session.on_key_down(now);
    if actions.iter().any(|a| *a == Action::Start) {
        guard.phase = VOICE_LISTENING;
        guard.pending = None;
        guard.final_text.clear();
        guard.error.clear();
        guard.level = 0;
        *guard.audio_level.lock().unwrap() = 0.0;
        // 真正打开麦克风：起采集线程，边录边喂会话
        let shared = inner.inner.clone();
        drop(guard);
        if let Err(e) = start_capture(shared) {
            let mut g = inner.inner.lock().unwrap();
            g.phase = VOICE_ERROR;
            g.error = e;
            g.session.cancel();
            return 0;
        }
        return 1;
    }
    0
}

/// 键松开。返回 1 表示会话已结束（此时应取 `voice_final_text`）。
#[no_mangle]
pub unsafe extern "C" fn voice_key_up(handle: *mut VoiceHandle) -> c_int {
    if handle.is_null() {
        return 0;
    }
    let inner = unsafe { &*handle };
    let now = Instant::now();
    let mut guard = inner.inner.lock().unwrap();
    if !guard.session.is_recording() {
        return 0;
    }
    let actions = guard.session.on_key_up(now);
    if actions.iter().any(|a| *a == Action::Stop) {
        finish_session(&mut guard);
        return 1;
    }
    0
}

/// 周期驱动（点按模式静音超时、最长时长）。会话结束时返回 1。
#[no_mangle]
pub unsafe extern "C" fn voice_tick(handle: *mut VoiceHandle) -> c_int {
    if handle.is_null() {
        return 0;
    }
    let inner = unsafe { &*handle };
    let now = Instant::now();
    let mut guard = inner.inner.lock().unwrap();
    if !guard.session.is_recording() {
        return 0;
    }
    // 把采集回调攒下的 PCM 取出来，喂给会话（分帧 + VAD + ASR 缓冲）
    let pcm: Vec<i16> = {
        let mut buf = guard.capture_pcm.lock().unwrap();
        if buf.is_empty() {
            Vec::new()
        } else {
            std::mem::take(&mut *buf)
        }
    };
    if !pcm.is_empty() {
        if let Some(update) = guard.session.push_pcm(&pcm, now) {
            if !update.is_empty() {
                guard.pending = Some(update);
            }
        }
    }

    if let Some(Action::Stop) = guard.session.tick(now) {
        finish_session(&mut guard);
        return 1;
    }
    // 音量：采集回调写 raw 音量，这里换算成 0-100
    let raw = *guard.audio_level.lock().unwrap();
    guard.level = (raw * 100.0).clamp(0.0, 100.0) as c_int;
    0
}

/// 推入一段 16kHz 单声道 PCM（采集线程/测试用）。返回 1 表示有新文本。
#[no_mangle]
pub unsafe extern "C" fn voice_push_pcm(
    handle: *mut VoiceHandle,
    samples: *const i16,
    len: usize,
) -> c_int {
    if handle.is_null() || samples.is_null() || len == 0 {
        return 0;
    }
    let inner = unsafe { &*handle };
    let pcm = unsafe { std::slice::from_raw_parts(samples, len) };
    // 用 RMS 估音量（16bit 满幅 = 1.0）
    let sum: f64 = pcm.iter().map(|&s| (s as f64 / 32768.0).powi(2)).sum();
    let rms = (sum / len as f64).sqrt() as f32;
    *inner.inner.lock().unwrap().audio_level.lock().unwrap() =
        (rms * 6.0).clamp(0.0, 1.0);

    let now = Instant::now();
    let mut guard = inner.inner.lock().unwrap();
    if !guard.session.is_recording() {
        return 0;
    }
    if let Some(update) = guard.session.push_pcm(pcm, now) {
        if !update.is_empty() {
            guard.pending = Some(update);
            return 1;
        }
    }
    0
}

/// 取「边说边出字」的增量：返回 1 表示有新文本，`voice_take_update` 可取。
#[no_mangle]
pub unsafe extern "C" fn voice_take_partial(handle: *mut VoiceHandle) -> c_int {
    if handle.is_null() {
        return 0;
    }
    let inner = unsafe { &*handle };
    let guard = inner.inner.lock().unwrap();
    guard.pending.is_some() as c_int
}

/// 当前状态码（VOICE_*）
#[no_mangle]
pub unsafe extern "C" fn voice_phase(handle: *mut VoiceHandle) -> c_int {
    if handle.is_null() {
        return VOICE_IDLE;
    }
    let inner = unsafe { &*handle };
    let guard = inner.inner.lock().unwrap();
    guard.phase
}

/// 当前音量 0-100
#[no_mangle]
pub unsafe extern "C" fn voice_level(handle: *mut VoiceHandle) -> c_int {
    if handle.is_null() {
        return 0;
    }
    let inner = unsafe { &*handle };
    let guard = inner.inner.lock().unwrap();
    guard.level
}

/// 取走待上屏的增量文本（调用方负责 `voice_free_string`）。
///
/// 返回形如 `"<退格数>\t<插入文本>"` 的字符串；没有可用文本时返回 NULL。
#[no_mangle]
pub unsafe extern "C" fn voice_take_update(handle: *mut VoiceHandle) -> *mut c_char {
    if handle.is_null() {
        return std::ptr::null_mut();
    }
    let inner = unsafe { &*handle };
    let mut guard = inner.inner.lock().unwrap();
    let Some(update) = guard.pending.take() else {
        return std::ptr::null_mut();
    };
    let s = format!("{}\t{}", update.backspaces, update.insert);
    match CString::new(s) {
        Ok(c) => c.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// 取最终文本（调用方负责 `voice_free_string`）；没有时返回 NULL。
#[no_mangle]
pub unsafe extern "C" fn voice_final_text(handle: *mut VoiceHandle) -> *mut c_char {
    if handle.is_null() {
        return std::ptr::null_mut();
    }
    let inner = unsafe { &*handle };
    let guard = inner.inner.lock().unwrap();
    if guard.final_text.is_empty() {
        return std::ptr::null_mut();
    }
    match CString::new(guard.final_text.clone()) {
        Ok(c) => c.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// 取错误信息（调用方负责 `voice_free_string`）；没有时返回 NULL。
#[no_mangle]
pub unsafe extern "C" fn voice_error(handle: *mut VoiceHandle) -> *mut c_char {
    if handle.is_null() {
        return std::ptr::null_mut();
    }
    let inner = unsafe { &*handle };
    let guard = inner.inner.lock().unwrap();
    if guard.error.is_empty() {
        return std::ptr::null_mut();
    }
    match CString::new(guard.error.clone()) {
        Ok(c) => c.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// 释放本库返回的字符串
#[no_mangle]
pub unsafe extern "C" fn voice_free_string(s: *mut c_char) {
    if !s.is_null() {
        drop(unsafe { CString::from_raw(s) });
    }
}

/// 丢弃当前会话
#[no_mangle]
pub unsafe extern "C" fn voice_cancel(handle: *mut VoiceHandle) {
    if handle.is_null() {
        return;
    }
    let inner = unsafe { &*handle };
    let mut guard = inner.inner.lock().unwrap();
    cancel_session(&mut guard);
}

/// 当前是否在录（0/1）
#[no_mangle]
pub unsafe extern "C" fn voice_is_recording(handle: *mut VoiceHandle) -> c_int {
    if handle.is_null() {
        return 0;
    }
    let inner = unsafe { &*handle };
    let guard = inner.inner.lock().unwrap();
    guard.session.is_recording() as c_int
}

/// 当前模式：0=长按，1=点按
#[no_mangle]
pub unsafe extern "C" fn voice_mode(handle: *mut VoiceHandle) -> c_int {
    if handle.is_null() {
        return 0;
    }
    let inner = unsafe { &*handle };
    let guard = inner.inner.lock().unwrap();
    match guard.session.mode() {
        Mode::Hold => 0,
        Mode::Tap => 1,
    }
}

fn cancel_session(guard: &mut Inner) {
    guard
        .stop_capture
        .store(true, std::sync::atomic::Ordering::SeqCst);
    #[cfg(feature = "capture")]
    {
        guard.capture = None; // drop 关闭麦克风
    }
    guard.session.cancel();
    guard.pending = None;
    guard.final_text.clear();
    guard.phase = VOICE_IDLE;
    guard.level = 0;
    *guard.audio_level.lock().unwrap() = 0.0;
}

fn finish_session(guard: &mut Inner) {
    guard
        .stop_capture
        .store(true, std::sync::atomic::Ordering::SeqCst);
    #[cfg(feature = "capture")]
    {
        guard.capture = None; // 先关麦克风再识别
    }
    guard.phase = VOICE_RECOGNIZING;
    let (text, update) = guard.session.finish();
    guard.final_text = text;
    guard.pending = Some(update);
    guard.phase = VOICE_DONE;
    guard.level = 0;
    *guard.audio_level.lock().unwrap() = 0.0;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试环境没有麦克风：让 start_capture 直接返回，用 voice_push_pcm 手工喂。
    fn no_capture() {
        std::env::set_var("FCITX5_VOICE_TEST_NO_CAPTURE", "1");
    }

    fn new_handle(cmd: *const c_char) -> *mut VoiceHandle {
        no_capture();
        unsafe { voice_new(cmd, 300, 1500) }
    }

    #[test]
    fn not_ready_without_backend() {
        let h = new_handle(std::ptr::null());
        assert_eq!(unsafe { voice_ready(h) }, 0);
        unsafe { voice_free(h) };
    }

    #[test]
    fn key_down_without_backend_reports_error() {
        let h = new_handle(std::ptr::null());
        assert_eq!(unsafe { voice_key_down(h) }, 0);
        assert_eq!(unsafe { voice_phase(h) }, VOICE_ERROR);
        unsafe { voice_free(h) };
    }

    #[test]
    fn ready_with_backend() {
        let cmd = CString::new("true").unwrap();
        let h = new_handle(cmd.as_ptr());
        assert_eq!(unsafe { voice_ready(h) }, 1);
        unsafe { voice_free(h) };
    }

    #[test]
    fn cancel_returns_to_idle() {
        let cmd = CString::new("true").unwrap();
        let h = new_handle(cmd.as_ptr());
        unsafe { voice_key_down(h) };
        assert_eq!(unsafe { voice_is_recording(h) }, 1);
        unsafe { voice_cancel(h) };
        assert_eq!(unsafe { voice_phase(h) }, VOICE_IDLE);
        assert_eq!(unsafe { voice_is_recording(h) }, 0);
        unsafe { voice_free(h) };
    }

    #[test]
    fn full_cycle_with_pcm_and_true_backend() {
        // "cat" 后端：把 WAV 原样读进来 → 不是有效文本，但整个路径不应崩
        let cmd = CString::new("cat").unwrap();
        let h = new_handle(cmd.as_ptr());
        unsafe { voice_key_down(h) };
        let pcm = vec![0i16; 1600];
        unsafe { voice_push_pcm(h, pcm.as_ptr(), pcm.len()) };
        // 紧接着松开会被判成点按（<300ms），会话继续
        assert_eq!(unsafe { voice_key_up(h) }, 0);
        assert!(unsafe { voice_is_recording(h) } == 1);
        assert_eq!(unsafe { voice_mode(h) }, 1);
        unsafe { voice_cancel(h) };
        assert_eq!(unsafe { voice_phase(h) }, VOICE_IDLE);
        unsafe { voice_free(h) };
    }

    #[test]
    fn hold_release_finishes_immediately_when_long_enough() {
        let cmd = CString::new("cat").unwrap();
        let h = new_handle(cmd.as_ptr());
        unsafe { voice_key_down(h) };
        std::thread::sleep(std::time::Duration::from_millis(350));
        assert_eq!(unsafe { voice_key_up(h) }, 1);
        assert_eq!(unsafe { voice_phase(h) }, VOICE_DONE);
        unsafe { voice_free(h) };
    }
}
