//! 边说边上屏的差分：公共前缀比对，把「旧文本」改成「新文本」。
//!
//! 插件拿到的 `Update` 就是「退格 N 个、再插入 S」，直接喂 fcitx5 的
//! `deleteSurroundingText` + `commitString` 实现（或干脆只保留最终结果）。

/// 把当前屏幕上的文本改成 `target` 所需的最小增量
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Update {
    pub backspaces: usize,
    pub insert: String,
}

impl Update {
    pub fn is_empty(&self) -> bool {
        self.backspaces == 0 && self.insert.is_empty()
    }
}

/// 记录「已经上屏的文本」，每次 `update` 返回从它变到新文本的差分。
#[derive(Debug, Default)]
pub struct Typer {
    /// 当前已经打到屏幕上的文本
    on_screen: String,
}

impl Typer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn on_screen(&self) -> &str {
        &self.on_screen
    }

    /// 计算从当前屏幕文本变到 `target` 的增量，并更新内部状态
    pub fn update(&mut self, target: &str) -> Update {
        let common = common_prefix_bytes(&self.on_screen, target);
        // 退格以「字符」为单位（UTF-8 一个汉字算一个）
        let backspaces = self.on_screen[common..].chars().count();
        let insert = target[common..].to_string();
        self.on_screen = target.to_string();
        Update {
            backspaces,
            insert,
        }
    }

    /// 会话结束：清空记录（文本已经在屏幕上了）
    pub fn finish(&mut self) {
        self.on_screen.clear();
    }

    pub fn reset(&mut self) {
        self.on_screen.clear();
    }
}

/// 两个字符串的公共前缀字节数，且保证落在字符边界上
fn common_prefix_bytes(a: &str, b: &str) -> usize {
    let mut i = 0;
    let (ab, bb) = (a.as_bytes(), b.as_bytes());
    while i < ab.len() && i < bb.len() && ab[i] == bb[i] {
        i += 1;
    }
    // 回退到字符边界
    while i > 0 && !a.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_update_inserts_all() {
        let mut t = Typer::new();
        let u = t.update("你好");
        assert_eq!(u.backspaces, 0);
        assert_eq!(u.insert, "你好");
    }

    #[test]
    fn growing_text_only_inserts_suffix() {
        let mut t = Typer::new();
        t.update("你好");
        let u = t.update("你好世界");
        assert_eq!(u.backspaces, 0);
        assert_eq!(u.insert, "世界");
    }

    #[test]
    fn corrected_text_backspaces_then_inserts() {
        let mut t = Typer::new();
        t.update("你好世借");
        let u = t.update("你好世界");
        assert_eq!(u.backspaces, 1);
        assert_eq!(u.insert, "界");
    }

    #[test]
    fn shrinking_text_backspaces() {
        let mut t = Typer::new();
        t.update("你好世界");
        let u = t.update("你好");
        assert_eq!(u.backspaces, 2);
        assert_eq!(u.insert, "");
    }

    #[test]
    fn no_change_is_empty() {
        let mut t = Typer::new();
        t.update("abc");
        assert!(t.update("abc").is_empty());
    }

    #[test]
    fn prefix_boundary_is_char_safe() {
        // 多字节字符中途的字节不能被当成公共前缀
        let mut t = Typer::new();
        t.update("天");
        let u = t.update("天气");
        assert_eq!(u.backspaces, 0);
        assert_eq!(u.insert, "气");
    }

    #[test]
    fn finish_clears_state() {
        let mut t = Typer::new();
        t.update("abc");
        t.finish();
        let u = t.update("xyz");
        assert_eq!(u.backspaces, 0);
        assert_eq!(u.insert, "xyz");
    }
}
