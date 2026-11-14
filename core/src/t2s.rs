//! 繁 → 简 单字转换。
//!
//! whisper 用 `-l zh` 识别中文时输出繁体（模型本身如此），这里做一次单字映射，
//! 把结果转成简体。表来自 OpenCC（见 `core/data/t2s.tsv` 头部注释）。
//!
//! 只做单字替换，不处理词组歧义（如「頭髮」→「头发」、「發現」→「发现」都能对，
//! 因为繁体本身就区分了字形）。对语音输入的短句足够。

use std::collections::HashMap;
use std::sync::OnceLock;

const TABLE: &str = include_str!("../data/t2s.tsv");

fn table() -> &'static HashMap<char, char> {
    static M: OnceLock<HashMap<char, char>> = OnceLock::new();
    M.get_or_init(|| {
        let mut m = HashMap::with_capacity(2800);
        for line in TABLE.lines() {
            if line.starts_with('#') || line.is_empty() {
                continue;
            }
            let mut it = line.split('\t');
            let (Some(t), Some(s)) = (it.next(), it.next()) else {
                continue;
            };
            let (mut tc, mut sc) = (t.chars(), s.chars());
            if let (Some(a), Some(b), None, None) = (tc.next(), sc.next(), tc.next(), sc.next()) {
                m.insert(a, b);
            }
        }
        m
    })
}

/// 把繁体字符串转成简体（单字映射，未收录的字原样保留）。
pub fn to_simplified(text: &str) -> String {
    let m = table();
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        out.push(*m.get(&c).unwrap_or(&c));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_common_traditional() {
        assert_eq!(to_simplified("這是一個測試"), "这是一个测试");
        assert_eq!(to_simplified("說話的聲音識別"), "说话的声音识别");
    }

    #[test]
    fn keeps_simplified_unchanged() {
        assert_eq!(to_simplified("这是简体"), "这是简体");
    }

    #[test]
    fn keeps_punctuation_and_ascii() {
        assert_eq!(to_simplified("測試，abc 123。"), "测试，abc 123。");
    }

    #[test]
    fn keeps_unknown_chars() {
        // 繁简同形的字（如「字」「的」）保持不变
        assert_eq!(to_simplified("字"), "字");
    }

    #[test]
    fn table_loaded() {
        assert!(table().len() > 2000, "繁简表应有两千多条");
    }
}
