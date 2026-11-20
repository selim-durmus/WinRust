//! Code shared by the WinRust programs: the language switch (`t!`, `tf!`, `en!`), the NTFS MFT reader, Win+D.
//! Use it with `#[macro_use] extern crate winrust_common;` at the crate root.
//!
//! English by default; Russian when %ProgramData%\WinRust\lang.txt contains "ru" (the installer's EN/RU button writes it).
//! Programs read the file once at start; the one that flips the switch updates its own texts at once.
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU8, Ordering::Relaxed},
};

pub mod desktop;
pub mod mft;
pub mod reg;
pub mod settings;
pub mod shell;
pub mod style;

static LANG: AtomicU8 = AtomicU8::new(0); // 0 = not read yet, 1 = English, 2 = Russian

fn file() -> Option<PathBuf> {
    std::env::var_os("ProgramData").map(|p| PathBuf::from(p).join("WinRust").join("lang.txt"))
}

/// The current language at run time. In code that has unit tests use `en!()` instead: it is always Russian under `cargo test`.
pub fn english() -> bool {
    match LANG.load(Relaxed) {
        1 => true,
        2 => false,
        _ => {
            let ru = file().and_then(|f| std::fs::read_to_string(f).ok()).is_some_and(|s| s.trim().eq_ignore_ascii_case("ru"));
            LANG.store(if ru { 2 } else { 1 }, Relaxed);
            !ru
        }
    }
}

/// Switches this process and writes the choice for the other programs (they pick it up when they start).
pub fn set_english(en: bool) {
    LANG.store(if en { 1 } else { 2 }, Relaxed);
    if let Some(f) = file() {
        let _ = std::fs::create_dir_all(f.parent().unwrap());
        let _ = std::fs::write(f, if en { "en" } else { "ru" });
    }
}

/// `en!()`: true for English. `cfg!(test)` is checked in the calling crate, so its unit tests keep their Russian expectations.
#[macro_export]
macro_rules! en {
    () => {
        !cfg!(test) && $crate::english()
    };
}

/// `t!("русский", "english")`: a text in the current language.
#[macro_export]
macro_rules! t {
    ($ru:expr, $en:expr) => {
        if $crate::en!() { $en } else { $ru }
    };
}

/// `tf!("русский {}", "english {}", args...)`: format! in the current language (both templates are checked at compile time).
#[macro_export]
macro_rules! tf {
    ($ru:literal, $en:literal $(, $a:expr)* $(,)?) => {
        if $crate::en!() { format!($en $(, $a)*) } else { format!($ru $(, $a)*) }
    };
}

#[cfg(test)]
mod tests {
    #[test]
    fn tests_run_in_russian() {
        assert_eq!(t!("да", "yes"), "да");
        assert_eq!(tf!("{} шт", "{} pcs", 3), "3 шт");
    }
}
