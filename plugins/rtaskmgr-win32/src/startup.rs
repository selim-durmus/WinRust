//! Автозагрузка: ключи Run в реестре и папки Startup; включение/выключение через StartupApproved
//! (так же, как это делает штатный диспетчер задач).
use std::{fs, path::PathBuf, ptr::null_mut};
use winapi::shared::minwindef::HKEY;
use winapi::um::{
    sysinfoapi::GetSystemTimeAsFileTime,
    winnt::{KEY_READ, KEY_SET_VALUE, REG_BINARY, REG_EXPAND_SZ, REG_SZ},
    winreg::*,
};

const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN32: &str = r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Run";
const APPROVED: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved";

struct Source {
    label: &'static str,
    hklm: bool,
    run: Option<&'static str>, // None — папка автозагрузки
    approved: &'static str,
}

static SOURCES: std::sync::LazyLock<[Source; 5]> = std::sync::LazyLock::new(|| [
    Source { label: "Registry, current user", hklm: false, run: Some(RUN), approved: "Run" },
    Source { label: "Registry, all users", hklm: true, run: Some(RUN), approved: "Run" },
    Source { label: "Registry, all users (32-bit)", hklm: true, run: Some(RUN32), approved: "Run32" },
    Source { label: "Startup folder (user)", hklm: false, run: None, approved: "StartupFolder" },
    Source { label: "Startup folder (all users)", hklm: true, run: None, approved: "StartupFolder" },
]);

pub struct Item {
    pub name: String, // имя значения реестра или имя файла
    pub command: String,
    pub source: usize,
    pub enabled: bool,
}

impl Item {
    pub fn source_label(&self) -> &'static str {
        SOURCES[self.source].label
    }
    /// Для HKLM нужны права администратора.
    pub fn needs_admin(&self) -> bool {
        SOURCES[self.source].hklm
    }
    pub fn key(&self) -> String {
        format!("{}|{}", self.source, self.name)
    }
}

fn w(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn root(hklm: bool) -> HKEY {
    if hklm { HKEY_LOCAL_MACHINE } else { HKEY_CURRENT_USER }
}

fn open(r: HKEY, path: &str, access: u32) -> Option<HKEY> {
    let mut k: HKEY = null_mut();
    (unsafe { RegOpenKeyExW(r, w(path).as_ptr(), 0, access, &mut k) } == 0).then_some(k)
}

fn from_utf16_bytes(b: &[u8]) -> String {
    let u: Vec<u16> = b.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    String::from_utf16_lossy(&u).trim_end_matches('\0').to_string()
}

/// Строковые значения ключа: (имя, данные).
fn string_values(r: HKEY, path: &str) -> Vec<(String, String)> {
    let mut out = vec![];
    let Some(k) = open(r, path, KEY_READ) else { return out };
    for i in 0.. {
        let mut name = vec![0u16; 16384];
        let mut nlen = name.len() as u32;
        let mut data = vec![0u8; 32768];
        let mut dlen = data.len() as u32;
        let mut ty = 0u32;
        let rc = unsafe { RegEnumValueW(k, i, name.as_mut_ptr(), &mut nlen, null_mut(), &mut ty, data.as_mut_ptr(), &mut dlen) };
        if rc != 0 {
            break; // ERROR_NO_MORE_ITEMS или ошибка — дальше читать нечего
        }
        if ty == REG_SZ || ty == REG_EXPAND_SZ {
            out.push((String::from_utf16_lossy(&name[..nlen as usize]), from_utf16_bytes(&data[..dlen as usize])));
        }
    }
    unsafe { RegCloseKey(k) };
    out
}

/// Включён ли элемент: в StartupApproved выключенному соответствует нечётный первый байт (0x03).
fn approved_enabled(hklm: bool, kind: &str, name: &str) -> bool {
    let Some(k) = open(root(hklm), &format!(r"{APPROVED}\{kind}"), KEY_READ) else { return true };
    let mut buf = [0u8; 12];
    let mut len = buf.len() as u32;
    let rc = unsafe { RegQueryValueExW(k, w(name).as_ptr(), null_mut(), null_mut(), buf.as_mut_ptr(), &mut len) };
    unsafe { RegCloseKey(k) };
    rc != 0 || len == 0 || buf[0] & 1 == 0
}

fn startup_dir(hklm: bool) -> Option<PathBuf> {
    let var = if hklm { "ProgramData" } else { "APPDATA" };
    std::env::var_os(var).map(|p| PathBuf::from(p).join(r"Microsoft\Windows\Start Menu\Programs\Startup"))
}

pub fn list() -> Vec<Item> {
    let mut out = vec![];
    for (i, s) in SOURCES.iter().enumerate() {
        match s.run {
            Some(path) => {
                for (name, command) in string_values(root(s.hklm), path) {
                    let enabled = approved_enabled(s.hklm, s.approved, &name);
                    out.push(Item { name, command, source: i, enabled });
                }
            }
            None => {
                let Some(d) = startup_dir(s.hklm) else { continue };
                for e in fs::read_dir(d).into_iter().flatten().flatten() {
                    let name = e.file_name().to_string_lossy().into_owned();
                    if name.eq_ignore_ascii_case("desktop.ini") {
                        continue;
                    }
                    let enabled = approved_enabled(s.hklm, s.approved, &name);
                    out.push(Item { name, command: e.path().display().to_string(), source: i, enabled });
                }
            }
        }
    }
    out
}

/// Значение StartupApproved: 12 байт — флаг (02 включено / 03 выключено) + FILETIME выключения.
fn approved_value(enabled: bool, filetime: u64) -> [u8; 12] {
    let mut v = [0u8; 12];
    v[0] = if enabled { 2 } else { 3 };
    if !enabled {
        v[4..].copy_from_slice(&filetime.to_le_bytes());
    }
    v
}

/// Err(код Win32) — например 5 (нет прав) для HKLM без администратора.
pub fn set_enabled(it: &Item, on: bool) -> Result<(), u32> {
    let s = &SOURCES[it.source];
    let mut ft = unsafe { std::mem::zeroed() };
    unsafe { GetSystemTimeAsFileTime(&mut ft) };
    let value = approved_value(on, (ft.dwHighDateTime as u64) << 32 | ft.dwLowDateTime as u64);
    let mut k: HKEY = null_mut();
    let path = w(&format!(r"{APPROVED}\{}", s.approved));
    let rc = unsafe {
        RegCreateKeyExW(root(s.hklm), path.as_ptr(), 0, null_mut(), 0, KEY_SET_VALUE, null_mut(), &mut k, null_mut())
    };
    if rc != 0 {
        return Err(rc as u32);
    }
    let rc = unsafe { RegSetValueExW(k, w(&it.name).as_ptr(), 0, REG_BINARY, value.as_ptr(), value.len() as u32) };
    unsafe { RegCloseKey(k) };
    if rc == 0 { Ok(()) } else { Err(rc as u32) }
}

const SELF_NAME: &str = "RTaskMgr";

pub fn self_autostart() -> bool {
    string_values(HKEY_CURRENT_USER, RUN).iter().any(|(n, _)| n == SELF_NAME)
}

/// Добавляет/убирает саму программу в автозагрузке текущего пользователя (запуск сразу в трей).
pub fn set_self_autostart(on: bool, exe: &str) -> bool {
    let Some(k) = open(HKEY_CURRENT_USER, RUN, KEY_SET_VALUE) else { return false };
    let rc = if on {
        let v: Vec<u16> = format!("\"{exe}\" --tray").encode_utf16().chain(Some(0)).collect();
        unsafe { RegSetValueExW(k, w(SELF_NAME).as_ptr(), 0, REG_SZ, v.as_ptr() as *const u8, (v.len() * 2) as u32) }
    } else {
        unsafe { RegDeleteValueW(k, w(SELF_NAME).as_ptr()) }
    };
    unsafe { RegCloseKey(k) };
    rc == 0 || !on // удалить отсутствующее значение — тоже успех
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approved_flags() {
        assert_eq!(approved_value(true, 123)[..4], [2, 0, 0, 0]);
        let d = approved_value(false, 0x0102_0304_0506_0708);
        assert_eq!(d[0] & 1, 1); // выключено — нечётный байт, как читает approved_enabled
        assert_eq!(d[4..], 0x0102_0304_0506_0708u64.to_le_bytes());
    }

    /// Интеграционный: временное значение в HKCU\Run → отключить → включить → удалить.
    #[test]
    fn toggle_roundtrip_on_temp_registry_value() {
        use winapi::um::winnt::KEY_WRITE;
        let name = "rtaskmgr_test_dummy";
        let value: Vec<u16> = "notepad.exe".encode_utf16().chain(Some(0)).collect();
        let run = open(HKEY_CURRENT_USER, RUN, KEY_WRITE).expect("HKCU Run");
        unsafe { RegSetValueExW(run, w(name).as_ptr(), 0, REG_SZ, value.as_ptr() as *const u8, (value.len() * 2) as u32) };
        let find = |on: bool| list().into_iter().any(|i| i.name == name && i.source == 0 && i.enabled == on);
        let result = std::panic::catch_unwind(|| {
            assert!(find(true), "the new value must be enabled");
            let it = list().into_iter().find(|i| i.name == name).unwrap();
            set_enabled(&it, false).unwrap();
            assert!(find(false), "после set_enabled(false) значение отключено");
            set_enabled(&it, true).unwrap();
            assert!(find(true), "после set_enabled(true) значение снова включено");
        });
        unsafe { RegDeleteValueW(run, w(name).as_ptr()) };
        if let Some(k) = open(HKEY_CURRENT_USER, &format!(r"{APPROVED}\Run"), KEY_SET_VALUE) {
            unsafe { RegDeleteValueW(k, w(name).as_ptr()) };
            unsafe { RegCloseKey(k) };
        }
        unsafe { RegCloseKey(run) };
        result.unwrap();
    }

    #[test]
    fn utf16_bytes_decode() {
        assert_eq!(from_utf16_bytes(&[0x41, 0, 0x42, 0, 0, 0]), "AB");
    }
}
