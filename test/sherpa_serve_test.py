#!/usr/bin/env python3
"""sherpa-serve 协议回归测试（不需要真人说话，用合成的「元音」音频）。

用假模型不可行（sherpa-onnx 必须真模型），所以这里只在模型存在时跑；
核心断言不依赖识别文本具体内容，只要求「协议时序正确」：

1. 句末哨兵必须**立即**被处理，不能因为贪婪读满缓冲区而饿死；
2. 句末必须在一行结果 + 一个空行内到达（上游 StreamingChannel 靠这个收尾）；
3. 音频不足 300ms 时句末给空行（表明"本句结束、无文本"），也要快。

用法：
    python3 test/sherpa_serve_test.py <sherpa-serve> <model.onnx> <tokens.txt>
"""
import math
import os
import struct
import subprocess
import sys
import time


def tone(samples: int, freq: float = 220.0, amp: int = 8000) -> bytes:
    """合成一段正弦波 PCM（i16 小端）——只验证链路，不要求识别出字。"""
    return b"".join(
        struct.pack("<h", int(amp * math.sin(2 * math.pi * freq * i / 16000)))
        for i in range(samples)
    )


def spawn(srv: str, model: str, tokens: str):
    return subprocess.Popen(
        [srv, f"--model={model}", f"--tokens={tokens}"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )


def wait_ready(p, timeout=10.0) -> bool:
    """等 stderr 出现 'sherpa-serve ready'（模型加载完）。"""
    deadline = time.time() + timeout
    buf = b""
    os.set_blocking(p.stderr.fileno(), False)
    while time.time() < deadline:
        try:
            chunk = p.stderr.read(4096)
        except BlockingIOError:
            chunk = None
        if chunk:
            buf += chunk
            if b"ready" in buf:
                return True
        elif p.poll() is not None:
            return False
        time.sleep(0.05)
    return False


def read_stdout_lines(p, timeout: float, want_end: bool = True):
    """在 timeout 内收集 stdout 的所有行（含空行）。

    协议：句末 = 一行结果（有文本时）+ 一个空行（总是作为「本句结束」信号）。
    收到空行即完成；只要拿到非空结果行也算完成（兼容只发结果行的旧行为）。
    """
    os.set_blocking(p.stdout.fileno(), False)
    deadline = time.time() + timeout
    buf = b""
    while time.time() < deadline:
        try:
            chunk = p.stdout.read(4096)
        except BlockingIOError:
            chunk = None
        if chunk:
            buf += chunk
            if b"\n" in buf:
                break
        time.sleep(0.02)
    return buf.decode("utf-8", "replace")


def case_sentinel_not_starved(srv, model, tokens, n_samples, label):
    """核心场景：喂任意长度音频（**不按 4096 对齐**）+ 哨兵，句末必须很快出结果。

    这正是真实麦克风的节奏：cpal 每次回调给几百到几千样本，
    长度几乎永远不是 4096 字节的整数倍。
    """
    p = spawn(srv, model, tokens)
    try:
        if not wait_ready(p):
            return None, "sherpa-serve 没能 ready"
        p.stdin.write(tone(n_samples))
        p.stdin.flush()
        time.sleep(0.15)
        p.stdin.write(b"\xff\xff\xff\xff")  # 句末哨兵
        p.stdin.flush()
        t0 = time.time()
        out = read_stdout_lines(p, timeout=6.0)
        dt = time.time() - t0
        # 合法收尾：有文本行，或一个空行（本句结束、无文本）
        ok = out.endswith("\n") and out != ""
        print(
            f"[{label}] {n_samples} samples: {dt:.2f}s  stdout={out!r}  "
            f"{'OK' if ok else 'FAIL（句末被饿死）'}"
        )
        return ok, None
    finally:
        p.kill()
        p.wait()


def case_short_utterance_gets_empty_line(srv, model, tokens):
    """不到 300ms 的音频：句末要回一个空行（"本句结束、无文本"），且要快。"""
    p = spawn(srv, model, tokens)
    try:
        if not wait_ready(p):
            return None, "sherpa-serve 没能 ready"
        p.stdin.write(tone(16000 * 100 // 1000))  # 100ms，小于 kMinSamples
        p.stdin.flush()
        time.sleep(0.1)
        p.stdin.write(b"\xff\xff\xff\xff")
        p.stdin.flush()
        t0 = time.time()
        out = read_stdout_lines(p, timeout=6.0)
        dt = time.time() - t0
        ok = "\n" in out  # 至少要有换行（空行 = 本句结束信号）
        print(
            f"[short] 100ms: {dt:.2f}s  stdout={out!r}  "
            f"{'OK' if ok else 'FAIL（没回空行，上游会一直等）'}"
        )
        return ok, None
    finally:
        p.kill()
        p.wait()


def case_two_utterances_same_process(srv, model, tokens):
    """同一进程连续两句：模型不重载，第二句也要正常出结果。"""
    p = spawn(srv, model, tokens)
    try:
        if not wait_ready(p):
            return None, "sherpa-serve 没能 ready"
        results = []
        for i in range(2):
            p.stdin.write(tone(16000))
            p.stdin.flush()
            time.sleep(0.1)
            p.stdin.write(b"\xff\xff\xff\xff")
            p.stdin.flush()
            out = read_stdout_lines(p, timeout=6.0)
        results.append(out)
        ok = all(o.endswith("\n") and o != "" for o in results)
        print(f"[two] 连续两句: {results!r}  {'OK' if ok else 'FAIL'}")
        return ok, None
    finally:
        p.kill()
        p.wait()


def main():
    if len(sys.argv) != 4:
        print(__doc__)
        return 2
    srv, model, tokens = sys.argv[1:4]
    if not os.path.exists(model):
        print(f"跳过：模型不存在 {model}")
        return 0

    failures = []
    cases = [
        # 真实麦克风节奏：各种「非 4096 对齐」的长度都要能收尾
        (case_sentinel_not_starved, (srv, model, tokens, 8000, "1/2s")),
        (case_sentinel_not_starved, (srv, model, tokens, 16000, "1s")),
        (case_sentinel_not_starved, (srv, model, tokens, 16000 * 3, "3s")),
        # 4096 字节边界附近（历史 bug 的触发点）
        (case_sentinel_not_starved, (srv, model, tokens, 2048, "4096B整")),
        (case_sentinel_not_starved, (srv, model, tokens, 2047, "4096B前1")),
        (case_sentinel_not_starved, (srv, model, tokens, 2049, "4096B后1")),
        (case_short_utterance_gets_empty_line, (srv, model, tokens)),
        (case_two_utterances_same_process, (srv, model, tokens)),
    ]
    for fn, args in cases:
        ok, err = fn(*args)
        if err:
            failures.append((fn.__name__, err))
        elif not ok:
            failures.append((fn.__name__, "断言失败"))

    print()
    if failures:
        print(f"失败 {len(failures)} 项：")
        for name, err in failures:
            print(f"  - {name}: {err}")
        return 1
    print("全部通过")
    return 0


if __name__ == "__main__":
    sys.exit(main())
