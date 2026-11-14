// fcitx5-voice：本地语音输入的 fcitx5 插件。
//
// 这是一个「独立模式」addon，不是输入法引擎：它不参与拼音/候选，只在
// PreInputMethod 阶段拦截语音热键（默认右 Alt），按住时录音、松开时上屏，
// 其余按键照常交给当前输入法。
//
//   fcitx5 事件循环 --(PreInputMethod)--> 本插件 --(C ABI)--> Rust 核心
//                                                        |-- 录音 cpal
//                                                        |-- 识别（外部命令/本地引擎）
//                                                        `-- 增量文本 -> commitString

#pragma once

#include <fcitx-config/configuration.h>
#include <fcitx-config/option.h>
#include <fcitx/addonfactory.h>
#include <fcitx/addonmanager.h>
#include <fcitx/addoninstance.h>
#include <fcitx/event.h>
#include <fcitx/inputcontext.h>
#include <fcitx/instance.h>
#include <fcitx-utils/event.h>

#include <memory>
#include <string>

#include "fcitx5-voice.h"

namespace fcitx {

class VoiceConfig : public Configuration {
public:
    Option<std::string> backendCommand{
        this, "Voice/BackendCommand", "识别命令（末尾自动追加 WAV 路径；空=关闭）",
        // 真正的默认值来自 share/fcitx5/conf/voice.conf（nix 包写死 whisper-cli
        // 与模型的绝对路径）；这里的兜底只在配置文件缺失时用。
        "whisper-cli -m /usr/share/whisper/ggml-base.bin -l zh -nt -f"};
    Option<std::string> voiceKey{this, "Voice/VoiceKey",
                                 "语音键（左 Alt / 右 Alt / 右 Ctrl / 菜单键）",
                                 "Alt_R"};
    Option<int, IntConstrain> holdMs{
        this, "Voice/HoldMs", "长按/点按分界（毫秒）", 300, IntConstrain(50, 5000)};
    Option<int, IntConstrain> silenceMs{
        this, "Voice/SilenceMs", "点按后静音多久自动结束（毫秒）", 1500,
        IntConstrain(200, 10000)};
    Option<bool> showPreedit{this, "Voice/ShowPreedit",
                             "说话时在编辑区显示识别预览", true};
    Option<bool> enabled{this, "Voice/Enabled", "启用语音输入", true};

    const char *typeName() const override { return "VoiceConfig"; }
};

class VoiceAddon : public AddonInstance {
public:
    explicit VoiceAddon(Instance *instance);
    ~VoiceAddon() override;

    /// 重新读配置（改完键位/后端立刻生效）
    void reloadConfig() override;
    const Configuration *getConfig() const override { return &config_; }
    void setConfig(const RawConfig &config) override;

    /// 当前是否在录音（给别的 addon 或调试用）
    bool isRecording() { return active_; }

private:
    void handleKeyEvent(KeyEvent &event);
    void poll();
    void finishAndCommit(InputContext *ic);
    void cancelSession();
    void startTimer();
    void stopTimer();

    /// 当前按键是否就是我们配置的语音键
    bool isVoiceKey(const Key &key) const;
    /// 取出 update 里的 (退格数, 插入文本)
    void applyUpdate(InputContext *ic, const std::string &update);
    /// 在编辑区显示/更新预览
    void showPreedit(InputContext *ic, const std::string &text);
    void clearPreedit(InputContext *ic);

    Instance *instance_;
    VoiceConfig config_;

    VoiceHandle *voice_ = nullptr;
    /// 正在录音（按下后到结束）
    bool active_ = false;
    /// 保存开始说话时的输入框，结果只往它上屏
    InputContext *ic_ = nullptr;
    /// 轮询定时器
    std::unique_ptr<EventSourceTime> timer_;
    /// 编辑区里上一次的预览文本
    std::string preedit_;
    /// 事件监听句柄
    std::unique_ptr<HandlerTableEntry<EventHandler>> keyWatcher_;
};

class VoiceAddonFactory : public AddonFactory {
public:
    AddonInstance *create(AddonManager *manager) override {
        return new VoiceAddon(manager->instance());
    }
};

} // namespace fcitx
