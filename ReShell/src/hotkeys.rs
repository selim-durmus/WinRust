//! Replacement for desktop-hotkeys.ps1: Win+1/2/3 -> desktop 1/2/3, Win+` -> previous desktop, Win+E -> our file manager,
//! Win+D -> show desktop, Win+I -> Settings. Win+E/D/I are done here, not left to Explorer: with topbar (and rust-explorer)
//! Explorer's taskbar is hidden or gone and it stops answering them although it still holds the hotkeys.
//! No undocumented COM: the current desktop comes from the registry, switching sends Ctrl+Win+Left/Right
//! (Win is already held by the user). shortcut: relies on the system shortcuts being enabled.
use std::sync::atomic::{AtomicIsize, AtomicU32, AtomicUsize, Ordering::Relaxed};
use windows_sys::Win32::{Foundation::*, System::{LibraryLoader::GetModuleHandleW, Registry::*}, UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*}};

static PREV: AtomicUsize = AtomicUsize::new(usize::MAX);
static HOOK: AtomicIsize = AtomicIsize::new(0);
static HELD: AtomicU32 = AtomicU32::new(0); // the Win+letter key still down: auto-repeat must not toggle the desktop again and again

/// (current index, desktop count) from the registry.
fn desktops() -> Option<(usize, usize)> {
    let key: Vec<u16> = "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Explorer\\VirtualDesktops\0".encode_utf16().collect();
    let get = |name: &str| -> Option<Vec<u8>> {
        let n: Vec<u16> = format!("{name}\0").encode_utf16().collect();
        let mut buf = vec![0u8; 1024];
        let mut len = buf.len() as u32;
        let r = unsafe { RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), n.as_ptr(), RRF_RT_REG_BINARY, std::ptr::null_mut(), buf.as_mut_ptr().cast(), &mut len) };
        (r == 0).then(|| { buf.truncate(len as usize); buf })
    };
    index_in(&get("VirtualDesktopIDs")?, &get("CurrentVirtualDesktop")?)
}

fn index_in(ids: &[u8], cur: &[u8]) -> Option<(usize, usize)> {
    let i = ids.chunks_exact(16).position(|c| c == cur)?;
    Some((i, ids.len() / 16))
}

fn tap(vk: u16, up: bool) -> INPUT {
    let mut i: INPUT = unsafe { std::mem::zeroed() };
    i.r#type = INPUT_KEYBOARD;
    i.Anonymous.ki = KEYBDINPUT { wVk: vk, wScan: 0, dwFlags: if up { KEYEVENTF_KEYUP } else { 0 }, time: 0, dwExtraInfo: 0 };
    i
}

fn send(keys: &[INPUT]) {
    unsafe { SendInput(keys.len() as u32, keys.as_ptr(), size_of::<INPUT>() as i32) };
}

/// The program in a shell command line: "\"C:\\Program Files\\FilePilot\\FPilot.exe\" \"%1\"" -> C:\Program Files\FilePilot\FPilot.exe.
fn command_exe(cmd: &str) -> Option<&str> {
    let c = cmd.trim();
    let exe = match c.strip_prefix('"') {
        Some(rest) => rest.split('"').next()?,
        None => c.split_whitespace().next()?,
    };
    (!exe.is_empty()).then_some(exe)
}

/// The file manager the user set to open folders (HKCU\Software\Classes\Directory\shell\open\command, the way File Pilot,
/// Directory Opus or rust-explorer register themselves), unless it is Explorer or is gone.
fn folder_handler() -> Option<std::path::PathBuf> {
    let key: Vec<u16> = "Software\\Classes\\Directory\\shell\\open\\command\0".encode_utf16().collect();
    let mut buf = vec![0u16; 1024];
    let mut len = (buf.len() * 2) as u32;
    let r = unsafe { RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), std::ptr::null(), RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ, std::ptr::null_mut(), buf.as_mut_ptr().cast(), &mut len) };
    if r != 0 {
        return None;
    }
    let s = String::from_utf16_lossy(&buf[..(len as usize / 2).saturating_sub(1)]);
    let exe = std::path::PathBuf::from(command_exe(&s)?);
    let name = exe.file_name()?.to_string_lossy().to_lowercase();
    (name != "explorer.exe" && exe.is_file()).then_some(exe)
}

/// Win+E: the user's folder handler (File Pilot...), else rust-explorer from the user's own folder (where updates land
/// without admin) or next to us, else Explorer.
/// Null std handles: this process has no console (FreeConsole) and a child that inherits the invalid ones fails to start.
fn open_files() {
    use std::{path::PathBuf, process::{Command, Stdio}};
    let mine = std::env::var_os("LOCALAPPDATA").map(|b| PathBuf::from(b).join(r"plugin-launcher\bin\rust-explorer.exe"));
    let beside = std::env::current_exe().ok().map(|e| e.with_file_name("rust-explorer.exe"));
    let exe = folder_handler().or_else(|| [mine, beside].into_iter().flatten().find(|p| p.exists())).unwrap_or_else(|| "explorer.exe".into());
    let _ = Command::new(exe).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
}

fn open_settings() {
    winrust_common::settings::open("ms-settings:");
}

/// What Win+<key> does here instead of in Explorer.
fn win_action(vk: u32) -> Option<fn()> {
    match vk {
        0x45 => Some(open_files),                        // E
        0x44 => Some(winrust_common::desktop::toggle),   // D
        0x49 => Some(open_settings),                     // I
        _ => None,
    }
}

fn go(target: usize) {
    let Some((cur, count)) = desktops() else { return };
    if target == cur || target >= count { return }
    PREV.store(cur, Relaxed);
    let key = if target > cur { VK_RIGHT } else { VK_LEFT };
    for _ in 0..cur.abs_diff(target) {
        send(&[tap(VK_CONTROL, false), tap(key, false), tap(key, true), tap(VK_CONTROL, true)]);
    }
}

unsafe extern "system" fn hook(n: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    if n >= 0 {
        let k = unsafe { &*(l as *const KBDLLHOOKSTRUCT) };
        let win = unsafe { (GetAsyncKeyState(VK_LWIN as i32) | GetAsyncKeyState(VK_RWIN as i32)) as u16 & 0x8000 != 0 };
        if (w == WM_KEYUP as usize || w == WM_SYSKEYUP as usize) && HELD.load(Relaxed) == k.vkCode {
            HELD.store(0, Relaxed); // also when Win was let go first
        }
        if let (Some(act), true) = (win_action(k.vkCode), win && k.flags & LLKHF_INJECTED == 0) {
            // swallow it so Explorer's own handler (if it still answers) does not act as well
            if w == WM_KEYDOWN as usize && HELD.swap(k.vkCode, Relaxed) != k.vkCode {
                send(&[tap(0xE8, false), tap(0xE8, true)]); // Win release must not open Start
                std::thread::spawn(act);
            }
            return 1;
        }
        let id = match k.vkCode { 0x31..=0x33 => Some(k.vkCode as usize - 0x31), 0xC0 => Some(usize::MAX), _ => None };
        if let (Some(id), true, false) = (id, win, k.flags & LLKHF_INJECTED != 0) {
            if w == WM_KEYDOWN as usize {
                // unassigned key tap stops Win release from opening Start; switching runs off the hook thread
                send(&[tap(0xE8, false), tap(0xE8, true)]);
                std::thread::spawn(move || go(if id == usize::MAX { PREV.load(Relaxed) } else { id }));
            }
            return 1;
        }
    }
    unsafe { CallNextHookEx(HOOK.load(Relaxed) as _, n, w, l) }
}

pub fn run() -> i32 {
    let h = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook), GetModuleHandleW(std::ptr::null()), 0) };
    if h.is_null() { eprintln!("hook failed"); return 1 }
    HOOK.store(h as isize, Relaxed);
    let mut m: MSG = unsafe { std::mem::zeroed() };
    while unsafe { GetMessageW(&mut m, std::ptr::null_mut(), 0, 0) } > 0 {}
    0
}

#[cfg(test)]
mod tests {
    #[test]
    fn exe_of_a_command_line() {
        assert_eq!(super::command_exe(r#""C:\Program Files\FilePilot\FPilot.exe" "%1""#), Some(r"C:\Program Files\FilePilot\FPilot.exe"));
        assert_eq!(super::command_exe(r"C:\Tools\fm.exe %1"), Some(r"C:\Tools\fm.exe"));
        assert_eq!(super::command_exe("  "), None);
        assert_eq!(super::command_exe(r#""""#), None);
    }

    #[test]
    fn finds_index() {
        let ids = [[1u8; 16], [2; 16], [3; 16]].concat();
        assert_eq!(super::index_in(&ids, &[2; 16]), Some((1, 3)));
        assert_eq!(super::index_in(&ids, &[9; 16]), None);
    }
}
