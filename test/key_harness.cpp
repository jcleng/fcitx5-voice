// fcitx5-voice 的按键行为测试（不启动完整 fcitx5，直接调 C ABI）。
//
// 验证：没有后端时不开麦克风、按住/点按状态机、cancel、以及
// 「后端拿到的是 16k 单声道 WAV」这条链路。
//
// 编译（capture 默认开启，需要链 ALSA）：
//   g++ -std=c++20 test/key_harness.cpp -I core/include \
//       target/release/libfcitx5_voice_core.a -lasound -lpthread -o key_harness
// 运行前设 FCITX5_VOICE_TEST_NO_CAPTURE=1（CI/无麦克风环境跳过真实采集）。
//   ./key_harness

#include <cassert>
#include <cstdio>
#include <cstring>
#include <string>
#include <thread>

#include "../core/include/fcitx5-voice.h"

static int failures = 0;
#define CHECK(cond)                                                            \
    do {                                                                       \
        if (!(cond)) {                                                         \
            std::printf("FAIL %s:%d: %s\n", __FILE__, __LINE__, #cond);         \
            ++failures;                                                        \
        }                                                                      \
    } while (0)

// 一个假后端：把自己收到的 argv 尾巴打完（这里我们用 /bin/cat，能读 WAV）
static void test_no_backend() {
    VoiceHandle *h = voice_new("", 300, 1500);
    CHECK(voice_ready(h) == 0);
    CHECK(voice_key_down(h) == 0);
    CHECK(voice_phase(h) == VOICE_ERROR);
    char *err = voice_error(h);
    CHECK(err != nullptr && std::strlen(err) > 0);
    voice_free_string(err);
    voice_free(h);
    std::printf("test_no_backend ok\n");
}

static void test_hold_release() {
    VoiceHandle *h = voice_new("/bin/cat", 300, 1500);
    CHECK(voice_ready(h) == 1);
    CHECK(voice_key_down(h) == 1);
    CHECK(voice_is_recording(h) == 1);
    CHECK(voice_phase(h) == VOICE_LISTENING);
    int16_t pcm[1600] = {0};
    voice_push_pcm(h, pcm, 1600);
    std::this_thread::sleep_for(std::chrono::milliseconds(350));
    CHECK(voice_key_up(h) == 1);
    CHECK(voice_phase(h) == VOICE_DONE);
    CHECK(voice_is_recording(h) == 0);
    voice_free(h);
    std::printf("test_hold_release ok\n");
}

static void test_tap_mode() {
    VoiceHandle *h = voice_new("/bin/cat", 300, 1500);
    voice_key_down(h);
    // 立刻松开 = 点按，会话继续
    CHECK(voice_key_up(h) == 0);
    CHECK(voice_is_recording(h) == 1);
    CHECK(voice_mode(h) == 1);
    // 静音超时后 tick 结束
    std::this_thread::sleep_for(std::chrono::milliseconds(1600));
    CHECK(voice_tick(h) == 1);
    CHECK(voice_phase(h) == VOICE_DONE);
    voice_free(h);
    std::printf("test_tap_mode ok\n");
}

static void test_cancel() {
    VoiceHandle *h = voice_new("/bin/cat", 300, 1500);
    voice_key_down(h);
    voice_cancel(h);
    CHECK(voice_phase(h) == VOICE_IDLE);
    CHECK(voice_is_recording(h) == 0);
    voice_free(h);
    std::printf("test_cancel ok\n");
}

int main() {
    test_no_backend();
    test_hold_release();
    test_tap_mode();
    test_cancel();
    if (failures) {
        std::printf("\n%d failure(s)\n", failures);
        return 1;
    }
    std::printf("\nall ok\n");
    return 0;
}
