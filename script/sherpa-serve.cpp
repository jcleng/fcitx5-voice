// sherpa-serve：常驻 sherpa-onnx 识别器，实现 fcitx5-voice 的 stream: 协议。
//
// 动机：直接调 `sherpa-onnx-offline` 每次识别都要重新加载模型（SenseVoice int8
// 约 239MB），在「按住说、松开上屏」的交互里能明显感觉到。这里把
// `SherpaOnnxCreateOfflineRecognizer` 只做一次，之后循环读 stdin 的 PCM 直接解码。
//
// 协议（见 core/src/sherpa.rs 的 StreamingChannel）：
// - stdin：裸 16kHz 单声道 i16 小端 PCM（无 WAV 头），持续读；
// - stdout：每行 = 「当前这一句的识别结果」（覆盖式，不是增量）；
// - **句末哨兵**：4 字节 `0xFF 0xFF 0xFF 0xFF`（解成 i16 = -1，真实麦克风不会给出）
//   表示「这一句说完了」→ 立刻解码切句并输出最终行，**进程与模型保留**；
// - stdin EOF（上游退出）时才收尾退出。
//
// 哨兵而不是「关 stdin」是关键：关 stdin 就只能一句一进程，每句重新加载
// 239MB 模型，首字延迟很难看。

#include <fcntl.h>
#include <signal.h>
#include <unistd.h>

#include <atomic>
#include <cmath>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <vector>

#include "sherpa-onnx/c-api/c-api.h"

namespace {

constexpr int32_t kSampleRate = 16000;
// 累计这么长的音频就尝试解码一次（输出中间结果）
constexpr size_t kChunkSamples = kSampleRate * 2;  // 2s
// 太短的片段不解码
constexpr size_t kMinSamples = kSampleRate * 300 / 1000;  // 300ms

struct Args {
  std::string model;
  std::string tokens;
  std::string language = "auto";
  bool use_itn = true;
  int32_t num_threads = 2;
};

void usage(const char *prog) {
  std::fprintf(stderr,
               "用法: %s --model=<model.int8.onnx> --tokens=<tokens.txt> "
               "[--language=auto|zh|en|ja|ko|yue] [--itn=0|1] [--num-threads=2]\n"
               "\n"
               "从 stdin 读 16kHz 单声道 i16 小端 PCM，向 stdout 每行输出当前识别结果。\n",
               prog);
}

bool parse_bool(const std::string &v) {
  return v == "1" || v == "true" || v == "True" || v == "yes";
}

bool parse_args(int argc, char **argv, Args *a) {
  for (int i = 1; i < argc; ++i) {
    std::string s = argv[i];
    auto take = [&](const char *key, std::string *dst) -> bool {
      const std::string prefix = std::string("--") + key + "=";
      if (s.rfind(prefix, 0) == 0) {
        *dst = s.substr(prefix.size());
        return true;
      }
      return false;
    };
    std::string v;
    if (take("model", &a->model)) continue;
    if (take("tokens", &a->tokens)) continue;
    if (take("language", &a->language)) continue;
    if (take("sense-voice-language", &a->language)) continue;
    if (take("itn", &v)) { a->use_itn = parse_bool(v); continue; }
    if (take("sense-voice-use-itn", &v)) { a->use_itn = parse_bool(v); continue; }
    if (take("num-threads", &v)) { a->num_threads = std::atoi(v.c_str()); continue; }
    if (s == "-h" || s == "--help") { usage(argv[0]); return false; }
    std::fprintf(stderr, "未知参数: %s\n", s.c_str());
    usage(argv[0]);
    return false;
  }
  if (a->model.empty() || a->tokens.empty()) {
    std::fprintf(stderr, "缺少 --model 或 --tokens\n");
    usage(argv[0]);
    return false;
  }
  return true;
}

/// 读一次就返回，最多 want 字节。返回值 0 = EOF，>0 = 本次读到的字节数。
///
/// **不能贪婪读满**：句末哨兵只有 4 字节，如果这里循环到凑满 want 才返回，
/// 哨兵会一直躺在管道里等后面的字节，永远凑不满 → 句末永不处理 →
/// 上游一直拿不到结果（表现为「松开没字」），而上游在主线程等 → 输入法卡死。
/// 让 read 的自然边界决定处理时机，`leftover` 负责跨读的半样本/半个哨兵。
size_t read_once(int fd, char *buf, size_t want) {
  for (;;) {
    ssize_t n = ::read(fd, buf, want);
    if (n >= 0) return static_cast<size_t>(n);
    if (errno == EINTR) continue;  // 被信号打断，重试
    return 0;                      // 出错按 EOF 处理，收尾退出
  }
}

/// 输出一行识别结果（覆盖式语义由上游负责）
void emit_text(const std::string &text) {
  std::fwrite(text.data(), 1, text.size(), stdout);
  std::fputc('\n', stdout);
  std::fflush(stdout);  // 上游是逐行读的，必须立刻刷出去
}

/// 句末但没识别出文本：空行也是「本句结束」的信号
void emit_empty() {
  std::fputc('\n', stdout);
  std::fflush(stdout);
}

}  // namespace

int main(int argc, char **argv) {
  // 上游可能随时关掉 stdin/杀掉进程，别死在 SIGPIPE 上
  ::signal(SIGPIPE, SIG_IGN);

  Args args;
  if (!parse_args(argc, argv, &args)) return 1;

  SherpaOnnxOfflineRecognizerConfig config;
  std::memset(&config, 0, sizeof(config));
  config.feat_config.sample_rate = kSampleRate;
  config.feat_config.feature_dim = 80;
  config.model_config.sense_voice.model = args.model.c_str();
  config.model_config.sense_voice.language = args.language.c_str();
  config.model_config.sense_voice.use_itn = args.use_itn ? 1 : 0;
  config.model_config.tokens = args.tokens.c_str();
  config.model_config.num_threads = args.num_threads;
  config.model_config.provider = "cpu";
  config.decoding_method = "greedy_search";

  const SherpaOnnxOfflineRecognizer *recognizer =
      SherpaOnnxCreateOfflineRecognizer(&config);
  if (recognizer == nullptr) {
    std::fprintf(stderr, "创建识别器失败，检查模型与 tokens 路径\n");
    return 1;
  }
  std::fprintf(stderr, "sherpa-serve ready\n");
  std::fflush(stderr);

  // 本句到目前为止的全部音频。句末用它整段解码一次，不能只解最后一段。
  // （之前把 pending 在中间预览后清空，导致只识别最后不到 2 秒 —— 上屏为空就是这个原因。）
  std::vector<float> utterance;
  utterance.reserve(kChunkSamples * 8);
  // 上次预览时已解码到的位置，避免同一段音频重复解码
  size_t previewed = 0;

  std::vector<char> raw(4096);
  std::string leftover;  // 跨 read 边界的半个样本

  // 对 [0, utterance.size()) 整段解码并输出一行。force=true 时即使没文本也输出空行，
  // 让上游知道「本句结束」。
  auto decode_utterance = [&](bool force) {
    if (utterance.size() < kMinSamples) {
      if (force) emit_empty();
      return;
    }
    const SherpaOnnxOfflineStream *stream = SherpaOnnxCreateOfflineStream(recognizer);
    if (stream == nullptr) {
      if (force) emit_empty();
      return;
    }
    SherpaOnnxAcceptWaveformOffline(stream, kSampleRate, utterance.data(),
                                    static_cast<int32_t>(utterance.size()));
    SherpaOnnxDecodeOfflineStream(recognizer, stream);
    const SherpaOnnxOfflineRecognizerResult *result =
        SherpaOnnxGetOfflineStreamResult(stream);
    std::string text;
    if (result != nullptr && result->text != nullptr) {
      text = result->text;
      const auto b = text.find_first_not_of(" \t\r\n");
      const auto e = text.find_last_not_of(" \t\r\n");
      text = (b == std::string::npos) ? "" : text.substr(b, e - b + 1);
    }
    if (result != nullptr) SherpaOnnxDestroyOfflineRecognizerResult(result);
    SherpaOnnxDestroyOfflineStream(stream);
    if (!text.empty()) {
      emit_text(text);
    } else if (force) {
      emit_empty();
    }
  };

  while (true) {
    const size_t got = read_once(STDIN_FILENO, raw.data(), raw.size());
    if (got == 0) break;  // EOF：上游退出，收尾

    leftover.append(raw.data(), got);

    // 用游标推进，不要每样本 erase(0,2)：那样是 O(n²)，长句（十几秒）会让
    // 句末解码活活拖上几百毫秒到秒级。处理完再把已消费的部分一次性删掉。
    size_t pos = 0;
    while (leftover.size() - pos >= 2) {
      const auto *b = reinterpret_cast<const unsigned char *>(leftover.data()) + pos;
      const size_t avail = leftover.size() - pos;

      // 句末哨兵：4 个 0xFF（解成 i16 是 -1，真实麦克风不会连续给出）
      if (avail >= 4 && b[0] == 0xFF && b[1] == 0xFF && b[2] == 0xFF &&
          b[3] == 0xFF) {
        pos += 4;
        // 句末：用**整句**音频解码一次，这才是最终结果
        decode_utterance(true);
        utterance.clear();
        previewed = 0;
        continue;
      }
      // 前两字节是 FF 但不足 4 字节：可能是哨兵的前半，留着等下次读
      if (b[0] == 0xFF && b[1] == 0xFF && avail < 4) {
        break;
      }

      const int16_t sample = static_cast<int16_t>(
          static_cast<uint16_t>(b[0]) | (static_cast<uint16_t>(b[1]) << 8));
      pos += 2;
      utterance.push_back(static_cast<float>(sample) / 32768.0f);
    }
    if (pos > 0) leftover.erase(0, pos);

    // 边说边出字：攒够一段就对整句重新解码一次，覆盖上一次的预览。
    // 注意是「整句」而不是「新增片段」——否则长句会被切断。
    if (utterance.size() - previewed >= kChunkSamples) {
      decode_utterance(false);
      previewed = utterance.size();
    }
  }

  // 收尾：把整句再解一次（EOF 前可能还有没预览的部分）
  decode_utterance(true);

  SherpaOnnxDestroyOfflineRecognizer(recognizer);
  return 0;
}
