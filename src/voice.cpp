// fcitx5-voice 插件实现。

#include "voice.h"

#include <fcitx-utils/event.h>
#include <fcitx-utils/key.h>
#include <fcitx-utils/keysym.h>
#include <fcitx-config/iniparser.h>
#include <fcitx-utils/log.h>

// fcitx5 5.1 把 StandardPath 改名为 StandardPaths（枚举变成顶层的
// StandardPathsType），但仍在 iniparser.h 里保留了 StandardPath::Type
// 的兼容模板。所以统一用 StandardPath::Type：5.0 原生支持，5.1 走转换器。
// 5.1 里 StandardPath 标了 deprecated，在 CMakeLists.txt 里对本文件关掉
// -Wdeprecated-declarations（见那里的 target_compile_options）。
#include <fcitx-utils/standardpath.h>

#include <cstdio>
#include <cstdlib>
#include <cstring>

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
        voice_key_down(voice_);
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

void VoiceAddon::showPreedit(InputContext *ic, const std::string &text) {
    // 独立 addon 不持有一个输入法引擎，无法直接写编辑区 preedit
    // （那是 InputMethodEngine 的职责，会和当前 rime/pinyin 抢）。
    // 这里只记录，真正上屏在 finishAndCommit 里一次性 commitString。
    // 想要「边说边出字」的实时预览，需要接入一个候选/预览 UI（后续可扩展）。
    (void)ic;
    preedit_ = text;
}

void VoiceAddon::clearPreedit(InputContext *ic) {
    (void)ic;
    preedit_.clear();
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
        // 边说边出字：取中间结果，只为预览
        if (voice_take_partial(voice_)) {
            if (char *u = voice_take_update(voice_)) {
                // 预览：直接把增量拼起来显示
                const std::string s(u);
                voice_free_string(u);
                const auto tab = s.find('\t');
                if (tab != std::string::npos) {
                    const std::string insert = s.substr(tab + 1);
                    if (!insert.empty()) {
                        preedit_ += insert;
                    }
                }
                showPreedit(ic, preedit_);
            }
        }
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
    ic_ = nullptr;
    clearPreedit(ic);

    // 最终结果用 commitString 正式上屏
    if (char *t = voice_final_text(voice_)) {
        if (t[0] != '\0') {
            ic->commitString(t);
        }
        voice_free_string(t);
    }
    // 清掉残留的增量
    if (char *u = voice_take_update(voice_)) {
        voice_free_string(u);
    }
    preedit_.clear();
}

void VoiceAddon::cancelSession() {
    stopTimer();
    active_ = false;
    if (ic_) {
        clearPreedit(ic_);
    }
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
