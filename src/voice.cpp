// fcitx5-voice 插件实现。

#include "voice.h"

#include <fcitx-utils/event.h>
#include <fcitx-utils/key.h>
#include <fcitx-utils/keysym.h>
#include <fcitx-config/iniparser.h>
#include <fcitx-utils/log.h>
#include <fcitx/inputpanel.h>
#include <fcitx/text.h>
#include <fcitx/userinterface.h>

// fcitx5 5.1 把 StandardPath 改名为 StandardPaths（枚举变成顶层的
// StandardPathsType），但仍在 iniparser.h 里保留了 StandardPath::Type
// 的兼容模板。所以统一用 StandardPath::Type：5.0 原生支持，5.1 走转换器。
// 5.1 里 StandardPath 标了 deprecated，在 CMakeLists.txt 里对本文件关掉
// -Wdeprecated-declarations（见那里的 target_compile_options）。
#include <fcitx-utils/standardpath.h>

#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <algorithm>

FCITX_DEFINE_LOG_CATEGORY(voice, "voice")

#define VLOG() FCITX_LOGC(voice, Info)
#define VWARN() FCITX_LOGC(voice, Warn)

namespace fcitx {

namespace {

/// 环境变量覆盖（调试用）
std::string envOr(const char *name, const std::string &fallback) {
    if (const char *v = std::getenv(name); v && *v) {
        return v;
    }
    return fallback;
}

int envIntOr(const char *name, int fallback) {
    if (const char *v = std::getenv(name); v && *v) {
        return std::atoi(v);
    }
    return fallback;
}

/// 把配置里的语音键名换成 KeySym
KeySym parseVoiceKey(const std::string &name) {
    if (name == "Alt_R" || name == "RightAlt") {
        return FcitxKey_Alt_R;
    }
    if (name == "Alt_L" || name == "LeftAlt") {
        return FcitxKey_Alt_L;
    }
    if (name == "Control_R" || name == "RightCtrl") {
        return FcitxKey_Control_R;
    }
    if (name == "Menu") {
        return FcitxKey_Menu;
    }
    return FcitxKey_Alt_R;
}

/// overlay 提示：录音中的音量波形用一组竖线字符画（UTF-8，每字 3 字节）
constexpr char kBlocks[] = "▁▂▃▄▅▆▇█";
/// 历史音量（0-8），让波形是短小的水平滚动条而不是单个跳动的字
constexpr int kBarCount = 8;
constexpr int kBlockLevels = 8;

} // namespace

VoiceAddon::VoiceAddon(Instance *instance) : instance_(instance) {
    reloadConfig();

    // 关键：PreInputMethod 让我们先于当前输入法（rime/pinyin）看到按键，
    // 这就是 fcitx5 官方为「独立模式 addon」留的口子。
    keyWatcher_ = instance_->watchEvent(
        EventType::InputContextKeyEvent, EventWatcherPhase::PreInputMethod,
        [this](Event &event) { handleKeyEvent(static_cast<KeyEvent &>(event)); });
}

VoiceAddon::~VoiceAddon() {
    stopTimer();
    if (voice_) {
        voice_free(voice_);
        voice_ = nullptr;
    }
}

void VoiceAddon::reloadConfig() {
    // 先读包内默认值（share/fcitx5/conf/voice.conf，由 nix postInstall 写入正确的
    // whisper-cli 与模型绝对路径），再读用户配置（~/.config/fcitx5/conf/voice.conf）覆盖。
    // 这样 store 路径不必编进 .so——Nix 对二进制做 placeholder 替换时长度变化会错位。
    // 配置文件里选项在 [Voice] 段下；Configuration::load 按 option 的顶层 path
    // 查找，所以先把该子段取出来再 load。
    auto loadSection = [this](StandardPath::Type type, bool partial) {
        RawConfig raw;
        readAsIni(raw, type, "conf/voice.conf");
        auto section = raw.get("Voice");
        if (section) {
            config_.load(*section, partial);
        } else if (!partial) {
            config_.load(RawConfig());
        }
    };
    loadSection(StandardPath::Type::PkgData, /* partial= */ false);
    loadSection(StandardPath::Type::PkgConfig, /* partial= */ true);

    // 环境变量优先，方便不写配置文件也能调
    std::string cmd = envOr("FCITX5_VOICE_BACKEND",
                            config_.backendCommand.value());
    const uint32_t hold =
        static_cast<uint32_t>(envIntOr("FCITX5_VOICE_HOLD_MS",
                                       config_.holdMs.value()));
    const uint32_t silence =
        static_cast<uint32_t>(envIntOr("FCITX5_VOICE_SILENCE_MS",
                                       config_.silenceMs.value()));

    if (voice_) {
        voice_free(voice_);
        voice_ = nullptr;
    }
    voice_ = voice_new(cmd.c_str(), hold, silence);
    VLOG() << "voice backend = " << cmd << ", ready = " << (voice_ ? voice_ready(voice_) : 0);
}

void VoiceAddon::setConfig(const RawConfig &config) {
    config_.load(config, true);
    safeSaveAsIni(config_, "conf/voice.conf");
    reloadConfig();
}

bool VoiceAddon::isVoiceKey(const Key &key) const {
    const KeySym want = parseVoiceKey(config_.voiceKey.value());
    if (key.sym() == want) {
        return true;
    }
    // 有些布局把右 Alt 报成 ISO_Level3_Shift（AltGr）
    if (want == FcitxKey_Alt_R && key.sym() == FcitxKey_ISO_Level3_Shift) {
        return true;
    }
    return false;
}

void VoiceAddon::handleKeyEvent(KeyEvent &event) {
    if (!config_.enabled.value() || !voice_) {
        return;
    }
    auto *ic = event.inputContext();

    if (isVoiceKey(event.key())) {
        if (event.isRelease()) {
            if (!active_) {
                return;
            }
            // 松开：长按模式下这里会话就结束了；点按模式继续录
            event.filterAndAccept();
            if (voice_key_up(voice_)) {
                finishAndCommit(ic);
            } else {
                // 点按模式：保持 active_，靠定时器等静音
                startTimer();
            }
            return;
        }

        // 按住时的自动重复：吃掉，不重复触发
        if (active_) {
            event.filterAndAccept();
            return;
        }

        event.filterAndAccept();
        if (!voice_ready(voice_)) {
            // 没有后端：只提示，不开麦克风
            return;
        }
        active_ = true;
        ic_ = ic;
        preedit_.clear();
        history_.fill(0);
        voice_key_down(voice_);
        // 立刻显示「正在听」，不等第一个 poll
        showIndicator(ic, indicatorText(VOICE_LISTENING, 0));
        startTimer();
        return;
    }

    // 说话途中按别的键：不要这次语音，键照常处理
    if (active_) {
        if (event.isRelease()) {
            return;
        }
        if (event.key().sym() == FcitxKey_Escape) {
            cancelSession();
            event.filterAndAccept();
            return;
        }
        if (event.key().isModifier()) {
            return;
        }
        cancelSession();
        // 让这个键继续往下走
    }
}

void VoiceAddon::applyUpdate(InputContext *ic, const std::string &update) {
    if (!ic || update.empty()) {
        return;
    }
    // update 形如 "<退格数>\t<插入文本>"
    const auto tab = update.find('\t');
    if (tab == std::string::npos) {
        return;
    }
    const int back = std::atoi(update.substr(0, tab).c_str());
    const std::string insert = update.substr(tab + 1);
    if (back > 0) {
        ic->deleteSurroundingText(-back, static_cast<unsigned>(back));
    }
    if (!insert.empty()) {
        ic->commitString(insert);
    }
}

std::string VoiceAddon::indicatorText(int phase, int level) const {
    if (phase == VOICE_RECOGNIZING) {
        return "⏳ 识别中…";
    }
    // 音量 0-100 → 8 档竖条，多根拼成一小段波形
    if (level < 0) {
        level = 0;
    }
    if (level > 100) {
        level = 100;
    }
    std::string bars;
    bars.reserve(kBarCount * 3 + 16);
    for (int i = 0; i < kBarCount; i++) {
        // 权重递增：最右边（最新音量）满高，越往左（越旧）越矮
        const int h = history_[i] * (i + 1) / kBarCount;
        const char *b = kBlocks + h * 3; // UTF-8 每个 3 字节
        bars.append(b, 3);
    }
    return "🎙 正在听… " + bars;
}

void VoiceAddon::showIndicator(InputContext *ic, const std::string &text) {
#ifdef FCITX5_VOICE_OVERLAY_MESSAGE
    if (!ic || !config_.showPreedit.value()) {
        return;
    }
    // 去重：50ms 一轮，只有文字变了才刷 UI
    if (text == indicator_) {
        return;
    }
    indicator_ = text;
    ic->inputPanel().setOverlayMessage(Text(text));
    ic->updateUserInterface(UserInterfaceComponent::InputPanel);
#else
    // fcitx5 < 5.1.22 没有 setOverlayMessage，什么也不显示（功能其余部分照常）
    (void)ic;
    (void)text;
#endif
}

void VoiceAddon::clearIndicator(InputContext *ic) {
    // 会话结束前 ic_ 还在，先清再置空
    const bool shown = !indicator_.empty();
    indicator_.clear();
    preedit_.clear();
#ifdef FCITX5_VOICE_OVERLAY_MESSAGE
    if (shown && ic) {
        ic->inputPanel().setOverlayMessage(Text());
        ic->updateUserInterface(UserInterfaceComponent::InputPanel);
    }
#else
    (void)ic;
#endif
}

void VoiceAddon::startTimer() {
    if (timer_) {
        timer_->setNextInterval(50 * 1000); // 50ms
        return;
    }
    timer_ = instance_->eventLoop().addTimeEvent(
        CLOCK_MONOTONIC, now(CLOCK_MONOTONIC) + 50 * 1000, 0,
        [this](EventSourceTime *source, uint64_t) {
            poll();
            if (active_) {
                source->setNextInterval(50 * 1000);
                source->setOneShot();
                return true;
            }
            return false; // 会话结束后停止
        });
}

void VoiceAddon::stopTimer() {
    if (timer_) {
        timer_->setEnabled(false);
        timer_.reset();
    }
}

void VoiceAddon::poll() {
    if (!voice_ || !active_) {
        return;
    }
    auto *ic = ic_;
    if (!ic) {
        cancelSession();
        return;
    }
    const int tick = voice_tick(voice_);
    const int phase = voice_phase(voice_);

    if (phase == VOICE_LISTENING) {
        // 边说边出字：取中间结果。overlay 只能放纯文本、且会被当前输入法的
        // 候选让位，这里不显示中间结果，只更新波形提示。
        // 真流式后端（stream: 前缀）会在这里给出增量：直接打到焦点输入框，
        // 实现「边说边出字」。whisper / sherpa CLI 的 partial 恒为 None，
        // 走不到这里，仍然是松手后一次性上屏。
        if (voice_take_partial(voice_)) {
            if (char *u = voice_take_update(voice_)) {
                const std::string s(u);
                voice_free_string(u);
                applyUpdate(ic, s);
            }
        }
        // 波形历史整体左移，把当前音量压到右端
        std::move(history_.begin() + 1, history_.end(), history_.begin());
        int level = voice_level(voice_);
        if (level < 0) {
            level = 0;
        }
        if (level > 100) {
            level = 100;
        }
        history_[kBarCount - 1] = (level * (kBlockLevels - 1) + 99) / 100;
        showIndicator(ic, indicatorText(VOICE_LISTENING, level));
        return;
    }

    if (!tick && phase == VOICE_RECOGNIZING) {
        // 松开后等后端出结果，光标处留个「识别中」
        showIndicator(ic, indicatorText(VOICE_RECOGNIZING, 0));
        return;
    }

    if (tick || phase == VOICE_DONE) {
        finishAndCommit(ic);
    } else if (phase == VOICE_ERROR) {
        if (char *e = voice_error(voice_)) {
            VWARN() << "voice error: " << e;
            voice_free_string(e);
        }
        cancelSession();
    }
}

void VoiceAddon::finishAndCommit(InputContext *ic) {
    if (!voice_) {
        return;
    }
    stopTimer();
    active_ = false;
    if (!ic) {
        ic = ic_;
    }
    clearIndicator(ic); // 必须在 ic_ 置空前调，否则清不掉
    ic_ = nullptr;

    // 收尾时优先用增量（差分）：流式后端已经边说边上屏了一部分，
    // 这里只需要把「屏幕上的旧文本」改成「最终文本」，不会重复上屏。
    // 非流式后端（whisper / sherpa CLI）从没上过屏，增量就是全文。
    bool applied = false;
    if (char *u = voice_take_update(voice_)) {
        const std::string s(u);
        voice_free_string(u);
        // s 形如 "<退格>\t<插入>"；只有真的退格或插入才算应用过
        const auto tab = s.find('\t');
        const bool noop = tab != std::string::npos && tab + 1 == s.size() &&
                          std::atoi(s.substr(0, tab).c_str()) == 0;
        if (!s.empty() && !noop) {
            applyUpdate(ic, s);
            applied = true;
        }
    }
    if (!applied) {
        // 没有增量（例如识别失败返回空）时，退回整段上屏
        if (char *t = voice_final_text(voice_)) {
            if (t[0] != '\0') {
                ic->commitString(t);
            }
            voice_free_string(t);
        }
    }
    preedit_.clear();
}

void VoiceAddon::cancelSession() {
    stopTimer();
    active_ = false;
    clearIndicator(ic_);
    ic_ = nullptr;
    if (voice_) {
        voice_cancel(voice_);
    }
    preedit_.clear();
}

} // namespace fcitx

#ifdef FCITX_ADDON_FACTORY_V2
FCITX_ADDON_FACTORY_V2(voice, fcitx::VoiceAddonFactory)
#else
FCITX_ADDON_FACTORY(fcitx::VoiceAddonFactory)
#endif
