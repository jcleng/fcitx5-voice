/*
 * fcitx5-voice：本地语音输入核心的 C ABI。
 *
 * 由 Rust 侧 core/src/ffi.rs 实现。插件（C++）只需按 50ms 周期轮询：
 *
 *   voice_key_down(h)                 // 热键按下
 *   voice_key_up(h)                   // 热键松开；返回 1 表示会话结束
 *   voice_tick(h)                     // 周期驱动（点按静音超时 / 最长时长）
 *   voice_take_update(h)              // 取走「边说边出字」的增量
 *   voice_cancel(h)                   // 丢弃本次（Esc / 打字 / 失焦）
 *
 * 所有函数都不阻塞。
 */
#ifndef FCITX5_VOICE_FFI_H
#define FCITX5_VOICE_FFI_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* 语音状态 */
#define VOICE_IDLE 0
#define VOICE_LISTENING 1
#define VOICE_RECOGNIZING 2
#define VOICE_DONE 3
#define VOICE_ERROR 4

typedef struct VoiceHandle VoiceHandle;

/*
 * 创建句柄。
 *   backend_cmd  识别命令行，末尾会自动追加 WAV 路径；空串表示没有后端
 *   hold_ms      长短按分界（毫秒），0 用默认 300
 *   silence_ms   点按模式的静音超时（毫秒），0 用默认 1500
 */
VoiceHandle *voice_new(const char *backend_cmd, uint32_t hold_ms,
                       uint32_t silence_ms);

/* 释放句柄 */
void voice_free(VoiceHandle *handle);

/* 有没有配置识别后端（0=没有，1=有）。没有时不该开麦克风。 */
int voice_ready(VoiceHandle *handle);

/* 键按下：返回 1 表示开始了一个新会话 */
int voice_key_down(VoiceHandle *handle);

/* 键松开：返回 1 表示会话已结束（此时取 voice_final_text） */
int voice_key_up(VoiceHandle *handle);

/* 周期驱动：会话结束时返回 1 */
int voice_tick(VoiceHandle *handle);

/* 推入 16kHz 单声道 PCM（采集线程/测试用）；返回 1 表示有新文本 */
int voice_push_pcm(VoiceHandle *handle, const int16_t *samples, size_t len);

/* 是否有待上屏的增量文本（0/1） */
int voice_take_partial(VoiceHandle *handle);

/* 当前状态码（VOICE_*） */
int voice_phase(VoiceHandle *handle);

/* 当前音量 0-100 */
int voice_level(VoiceHandle *handle);

/*
 * 取走待上屏的增量文本，形如 "<退格数>\t<插入文本>"；没有时返回 NULL。
 * 返回的字符串用 voice_free_string 释放。
 */
char *voice_take_update(VoiceHandle *handle);

/* 取最终文本；没有时返回 NULL。用 voice_free_string 释放。 */
char *voice_final_text(VoiceHandle *handle);

/* 取错误信息；没有时返回 NULL。用 voice_free_string 释放。 */
char *voice_error(VoiceHandle *handle);

/* 释放本库返回的字符串 */
void voice_free_string(char *s);

/* 丢弃当前会话 */
void voice_cancel(VoiceHandle *handle);

/* 当前是否在录（0/1） */
int voice_is_recording(VoiceHandle *handle);

/* 当前模式：0=长按，1=点按 */
int voice_mode(VoiceHandle *handle);

#ifdef __cplusplus
}
#endif

#endif /* FCITX5_VOICE_FFI_H */
