//! A one-line text field's editing, as in a Windows edit box: a caret and a selection (anchor .. caret), arrows,
//! Home / End, Shift to select, Ctrl to jump or delete by word. Positions are char indices, not bytes.
//! Pure logic (no Win32): the Start search box keeps its text in QUERY and the caret / anchor beside it.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Class {
    Space,
    Word,
    Punct,
}

fn class(c: char) -> Class {
    if c.is_whitespace() {
        Class::Space
    } else if c.is_alphanumeric() || c == '_' {
        Class::Word
    } else {
        Class::Punct
    }
}

pub struct Field<'a> {
    pub text: &'a mut String,
    pub caret: usize,
    pub anchor: usize,
}

impl Field<'_> {
    fn chars(&self) -> Vec<char> {
        self.text.chars().collect()
    }
    fn len(&self) -> usize {
        self.text.chars().count()
    }
    /// The selection as (start, end); empty when start == end.
    pub fn sel(&self) -> (usize, usize) {
        (self.caret.min(self.anchor), self.caret.max(self.anchor))
    }
    pub fn selected(&self) -> String {
        let (a, b) = self.sel();
        self.chars()[a..b].iter().collect()
    }
    /// Keeps caret and anchor inside the text (after the text was changed from outside).
    pub fn clamp(&mut self) {
        let n = self.len();
        self.caret = self.caret.min(n);
        self.anchor = self.anchor.min(n);
    }
    fn replace(&mut self, a: usize, b: usize, with: &str) {
        let c = self.chars();
        let s: String = c[..a].iter().chain(with.chars().collect::<Vec<_>>().iter()).chain(c[b..].iter()).collect();
        *self.text = s;
        self.caret = a + with.chars().count();
        self.anchor = self.caret;
    }
    /// Typing or pasting: replaces the selection.
    pub fn insert(&mut self, s: &str) {
        let (a, b) = self.sel();
        self.replace(a, b, s);
    }
    /// Start of the word left of `i` (Ctrl+Left, Ctrl+Backspace): skips spaces, then one run of the same kind.
    pub fn word_left(&self, i: usize) -> usize {
        let c = self.chars();
        let mut i = i.min(c.len());
        while i > 0 && class(c[i - 1]) == Class::Space {
            i -= 1;
        }
        if i > 0 {
            let k = class(c[i - 1]);
            while i > 0 && class(c[i - 1]) == k {
                i -= 1;
            }
        }
        i
    }
    /// Start of the next word right of `i` (Ctrl+Right, Ctrl+Delete): skips one run, then the spaces after it.
    pub fn word_right(&self, i: usize) -> usize {
        let c = self.chars();
        let mut i = i.min(c.len());
        if i < c.len() && class(c[i]) != Class::Space {
            let k = class(c[i]);
            while i < c.len() && class(c[i]) == k {
                i += 1;
            }
        }
        while i < c.len() && class(c[i]) == Class::Space {
            i += 1;
        }
        i
    }
    fn go(&mut self, to: usize, extend: bool) {
        self.caret = to;
        if !extend {
            self.anchor = to;
        }
    }
    /// Left arrow. Without Shift a selection collapses to its start, like a Windows edit box.
    pub fn left(&mut self, word: bool, extend: bool) {
        let (a, b) = self.sel();
        if a != b && !extend && !word {
            return self.go(a, false);
        }
        let to = if word { self.word_left(self.caret) } else { self.caret.saturating_sub(1) };
        self.go(to, extend);
    }
    /// Right arrow. Without Shift a selection collapses to its end.
    pub fn right(&mut self, word: bool, extend: bool) {
        let (a, b) = self.sel();
        if a != b && !extend && !word {
            return self.go(b, false);
        }
        let to = if word { self.word_right(self.caret) } else { (self.caret + 1).min(self.len()) };
        self.go(to, extend);
    }
    pub fn home(&mut self, extend: bool) {
        self.go(0, extend);
    }
    pub fn end(&mut self, extend: bool) {
        let n = self.len();
        self.go(n, extend);
    }
    pub fn select_all(&mut self) {
        self.anchor = 0;
        self.caret = self.len();
    }
    /// Backspace: the selection, else one char (Ctrl: back to the word start). false = nothing changed.
    pub fn backspace(&mut self, word: bool) -> bool {
        let (a, b) = self.sel();
        if a != b {
            self.replace(a, b, "");
            return true;
        }
        let from = if word { self.word_left(self.caret) } else { self.caret.saturating_sub(1) };
        if from == self.caret {
            return false;
        }
        self.replace(from, self.caret, "");
        true
    }
    /// Delete: the selection, else one char to the right (Ctrl: up to the next word). false = nothing changed.
    pub fn delete(&mut self, word: bool) -> bool {
        let (a, b) = self.sel();
        if a != b {
            self.replace(a, b, "");
            return true;
        }
        let to = if word { self.word_right(self.caret) } else { (self.caret + 1).min(self.len()) };
        if to == self.caret {
            return false;
        }
        let c = self.caret;
        self.replace(c, to, "");
        true
    }
}

#[cfg(test)]
mod tests {
    use super::Field;

    fn f(s: &mut String, caret: usize) -> Field<'_> {
        Field { text: s, caret, anchor: caret }
    }

    #[test]
    fn word_jumps_like_windows() {
        let mut s = "visual studio  code".to_string();
        let e = f(&mut s, 19);
        assert_eq!(e.word_left(19), 15); // "code"
        assert_eq!(e.word_left(15), 7); // spaces skipped, "studio"
        assert_eq!(e.word_right(0), 7); // past "visual" and its space
        assert_eq!(e.word_right(7), 15); // past "studio" and both spaces
        assert_eq!(e.word_right(15), 19);
        let mut s = "1password-mcp".to_string();
        let e = f(&mut s, 13);
        assert_eq!(e.word_left(13), 10); // "mcp"
        assert_eq!(e.word_left(10), 9); // "-" is its own run
        assert_eq!(e.word_left(9), 0);
    }

    #[test]
    fn ctrl_backspace_and_delete() {
        let mut s = "open visual studio".to_string();
        let mut e = f(&mut s, 18);
        assert!(e.backspace(true));
        assert_eq!((e.text.as_str(), e.caret), ("open visual ", 12));
        assert!(e.backspace(true));
        assert_eq!((e.text.as_str(), e.caret), ("open ", 5));
        e.home(false);
        assert!(e.delete(true));
        assert_eq!((e.text.as_str(), e.caret), ("", 0));
        assert!(!e.backspace(false) && !e.delete(true)); // empty: nothing to do
    }

    #[test]
    fn shift_selects_and_typing_replaces() {
        let mut s = "chrome beta".to_string();
        let mut e = f(&mut s, 11);
        e.left(true, true); // Ctrl+Shift+Left: "beta"
        assert_eq!(e.selected(), "beta");
        e.insert("dev");
        assert_eq!((e.text.as_str(), e.caret, e.anchor), ("chrome dev", 10, 10));
        e.home(true); // Shift+Home
        assert_eq!(e.selected(), "chrome dev");
        e.right(false, false); // a plain arrow collapses the selection to its end
        assert_eq!((e.caret, e.anchor), (10, 10));
        e.select_all();
        assert!(e.backspace(false));
        assert_eq!(e.text.as_str(), "");
    }

    #[test]
    fn non_ascii_counts_chars() {
        let mut s = "телеграм ü".to_string();
        let mut e = f(&mut s, 10);
        e.left(true, false);
        assert_eq!(e.caret, 9);
        assert!(e.backspace(true));
        assert_eq!(e.text.as_str(), "ü");
    }
}
