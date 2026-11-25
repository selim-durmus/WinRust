//! Old graphics drivers. Windows keeps every display driver it ever installed in the driver store (an NVIDIA package is about
//! 3 GB), so that "Roll back driver" has something to go back to. Here: measure them, remove them, and an optional scheduled task
//! that removes them by itself.
//! "Old" = a Display-class driver package (oemNN.inf) that no device is set up with and that is not the newest one of its
//! provider. Removing goes through SetupUninstallOEMInf without the force flag, so Windows itself refuses a package in use.
//! Only documented calls (SetupAPI, Task Scheduler COM); removing and the task need administrator rights: the window starts this
//! exe again elevated (`--old-drivers clean|auto-on|auto-off`, one UAC prompt), the task runs `--old-drivers clean` as SYSTEM.
use crate::sys::{self, wide};
use std::{
    path::PathBuf,
    ptr::{null, null_mut},
    sync::{
        Mutex,
        atomic::{AtomicU8, Ordering::Relaxed},
    },
    time::Duration,
};
use windows_sys::Win32::{Devices::DeviceAndDriverInstallation::*, System::Registry::HKEY_LOCAL_MACHINE};

const DISPLAY: &str = "{4d36e968-e325-11ce-bfc1-08002be10318}";
const CLASS_KEY: &str = r"SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}";
const TASK: &str = r"\WinRust - remove old graphics drivers";

#[derive(Clone, Debug)]
pub struct Pkg {
    pub inf: String,      // oem72.inf
    pub provider: String, // NVIDIA
    pub version: String,  // 32.0.16.2124
    pub bytes: u64,       // its driver-store folder
    pub old: bool,        // safe to remove
}

/// None: not measured yet.
static FOUND: Mutex<Option<Vec<Pkg>>> = Mutex::new(None);
/// The scheduled task: 0 unknown, 1 off, 2 on.
static AUTO: AtomicU8 = AtomicU8::new(0);

fn inf_value(h: *const core::ffi::c_void, key: &str) -> String {
    let mut buf = [0u16; 512];
    let mut n = 0u32;
    let ok = unsafe { SetupGetLineTextW(null(), h, wide("Version").as_ptr(), wide(key).as_ptr(), buf.as_mut_ptr(), buf.len() as u32, &mut n) };
    if ok == 0 { String::new() } else { String::from_utf16_lossy(&buf[..(n as usize).saturating_sub(1)]) }
}

/// The driver-store folder of a published INF (`C:\Windows\INF\oem72.inf` -> `...\FileRepository\nv_dispi.inf_amd64_...`).
fn store_dir(inf: &std::path::Path) -> Option<PathBuf> {
    let mut buf = vec![0u16; 1024];
    let mut n = 0u32;
    let p = wide(&inf.to_string_lossy());
    if unsafe { SetupGetInfDriverStoreLocationW(p.as_ptr(), null(), null(), buf.as_mut_ptr(), buf.len() as u32, &mut n) } == 0 {
        return None;
    }
    let s = String::from_utf16_lossy(&buf[..(n as usize).saturating_sub(1)]);
    PathBuf::from(s).parent().map(|d| d.to_path_buf())
}

/// The INFs (lower case) devices of the Display class are set up with, present or not (an unplugged card keeps its driver).
fn in_use() -> Vec<String> {
    sys::subkeys(HKEY_LOCAL_MACHINE, CLASS_KEY)
        .iter()
        .filter_map(|k| sys::string(HKEY_LOCAL_MACHINE, &format!(r"{CLASS_KEY}\{k}"), "InfPath"))
        .map(|s| s.to_lowercase())
        .collect()
}

/// "11/07/2026,32.0.16.2124" -> the version as numbers for comparing.
fn version_key(v: &str) -> Vec<u32> {
    v.split('.').map(|p| p.trim().parse().unwrap_or(0)).collect()
}

/// Marks as old every package that is not in use and not the newest of its provider.
fn mark_old(pkgs: &mut [Pkg], used: &[String]) {
    for i in 0..pkgs.len() {
        let newest = pkgs.iter().filter(|q| q.provider == pkgs[i].provider).all(|q| version_key(&q.version) <= version_key(&pkgs[i].version));
        pkgs[i].old = !newest && !used.contains(&pkgs[i].inf.to_lowercase());
    }
}

/// Every third-party Display-class driver package, with its size (reads a few GB of folder entries: call it in a thread).
pub fn scan() -> Vec<Pkg> {
    let inf_dir = PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into())).join("INF");
    let mut out = vec![];
    let Ok(rd) = std::fs::read_dir(&inf_dir) else { return out };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_lowercase();
        if !(name.starts_with("oem") && name.ends_with(".inf")) {
            continue;
        }
        let h = unsafe { SetupOpenInfFileW(wide(&e.path().to_string_lossy()).as_ptr(), null(), INF_STYLE_WIN4, null_mut()) };
        if h.is_null() || h as isize == -1 {
            continue;
        }
        let (class, provider, ver) = (inf_value(h, "ClassGUID"), inf_value(h, "Provider"), inf_value(h, "DriverVer"));
        unsafe { SetupCloseInfFile(h) };
        if !class.eq_ignore_ascii_case(DISPLAY) {
            continue;
        }
        let version = ver.split(',').nth(1).unwrap_or("").trim().to_string();
        let bytes = store_dir(&e.path()).map_or(0, |d| crate::ext::dir_bytes(&d, Duration::ZERO));
        out.push(Pkg { inf: name, provider, version, bytes, old: false });
    }
    mark_old(&mut out, &in_use());
    out
}

/// Removes the old packages (administrator). Returns (removed, failed, bytes freed); each one goes into the log.
pub fn clean() -> (u32, u32, u64) {
    let (mut removed, mut failed, mut freed) = (0, 0, 0);
    for p in scan().into_iter().filter(|p| p.old) {
        let ok = unsafe { SetupUninstallOEMInfW(wide(&p.inf).as_ptr(), 0, null()) } != 0;
        let err = if ok { 0 } else { unsafe { windows_sys::Win32::Foundation::GetLastError() } };
        log(&format!("{} {} {} {}", p.inf, p.provider, p.version, if ok { format!("removed ({})", crate::power::size(p.bytes)) } else { format!("NOT removed (error {err})") }));
        if ok {
            (removed, freed) = (removed + 1, freed + p.bytes);
        } else {
            failed += 1;
        }
    }
    (removed, failed, freed)
}

/// One line in %ProgramData%\WinRust\old-drivers.log (what went, for anyone who wonders where a driver went).
fn log(line: &str) {
    use std::io::Write;
    let Some(pd) = std::env::var_os("ProgramData") else { return };
    let dir = PathBuf::from(pd).join("WinRust");
    let _ = std::fs::create_dir_all(&dir);
    let mut t: windows_sys::Win32::Foundation::SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut t) };
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("old-drivers.log")) {
        let _ = writeln!(f, "{:04}-{:02}-{:02} {:02}:{:02} {line}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute);
    }
}

// ---- what the window shows ----

/// Measures in a thread (also reads the task state); `done` runs afterwards.
pub fn measure(done: impl FnOnce() + Send + 'static) {
    std::thread::spawn(move || {
        let found = scan();
        *FOUND.lock().unwrap() = Some(found);
        AUTO.store(match task_exists() { Some(true) => 2, Some(false) => 1, None => 0 }, Relaxed);
        done();
    });
}

pub fn measured() -> bool {
    FOUND.lock().unwrap().is_some()
}

/// "2.9 GB (1)", "none", "measuring…".
pub fn text() -> String {
    match &*FOUND.lock().unwrap() {
        None => t!("считаю…", "measuring…").into(),
        Some(f) => {
            let old: Vec<_> = f.iter().filter(|p| p.old).collect();
            if old.is_empty() { t!("нет", "none").into() } else { format!("{} ({})", crate::power::size(old.iter().map(|p| p.bytes).sum()), old.len()) }
        }
    }
}

pub fn any_old() -> bool {
    FOUND.lock().unwrap().as_ref().is_some_and(|f| f.iter().any(|p| p.old))
}

/// The switch: None while unknown (drawn disabled).
pub fn auto_on() -> Option<bool> {
    match AUTO.load(Relaxed) {
        1 => Some(false),
        2 => Some(true),
        _ => None,
    }
}

// ---- administrator rights ----

pub fn elevated() -> bool {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE},
        Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation},
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };
    unsafe {
        let mut tok: HANDLE = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok) == 0 {
            return false;
        }
        let mut e = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut n = 0u32;
        let ok = GetTokenInformation(tok, TokenElevation, &mut e as *mut _ as _, size_of::<TOKEN_ELEVATION>() as u32, &mut n) != 0;
        CloseHandle(tok);
        ok && e.TokenIsElevated != 0
    }
}

/// This exe, only when it is the installed copy in Program Files (writable by administrators only): a task that runs as SYSTEM
/// must never start an exe a normal user could replace.
fn installed_exe() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let pf = PathBuf::from(std::env::var_os("ProgramFiles")?);
    exe.starts_with(&pf).then_some(exe)
}

/// Runs `--old-drivers <what>` as administrator (one UAC prompt) and waits. Some(exit code), None: refused or failed to start.
fn run_elevated(what: &str) -> Option<u32> {
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject},
        UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW},
    };
    let exe = wide(&std::env::current_exe().ok()?.to_string_lossy());
    let (verb, args) = (wide("runas"), wide(&format!("--old-drivers {what}")));
    unsafe {
        let mut si: SHELLEXECUTEINFOW = std::mem::zeroed();
        si.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
        si.fMask = SEE_MASK_NOCLOSEPROCESS;
        (si.lpVerb, si.lpFile, si.lpParameters, si.nShow) = (verb.as_ptr(), exe.as_ptr(), args.as_ptr(), 0);
        if ShellExecuteExW(&mut si) == 0 || si.hProcess.is_null() {
            return None;
        }
        WaitForSingleObject(si.hProcess, INFINITE);
        let mut code = 0u32;
        GetExitCodeProcess(si.hProcess, &mut code);
        CloseHandle(si.hProcess);
        Some(code)
    }
}

/// Exit code of `--old-drivers clean`: removed | failed << 8 (the freed size is measured again by the window).
fn clean_code() -> u32 {
    let (r, f, _) = clean();
    r.min(255) | f.min(255) << 8
}

/// The window's "remove now", in a thread; `done` gets the message.
pub fn clean_now(done: impl FnOnce(String) + Send + 'static) {
    std::thread::spawn(move || {
        let before: u64 = FOUND.lock().unwrap().iter().flatten().filter(|p| p.old).map(|p| p.bytes).sum();
        let code = if elevated() { Some(clean_code()) } else { run_elevated("clean") };
        *FOUND.lock().unwrap() = Some(scan());
        let after: u64 = FOUND.lock().unwrap().iter().flatten().filter(|p| p.old).map(|p| p.bytes).sum();
        done(match code {
            None => t!("Отменено: нужны права администратора", "Cancelled: needs administrator rights").into(),
            Some(c) if c >> 8 & 255 > 0 => tf!("Удалено: {}, не удалось: {} (см. old-drivers.log)", "Removed: {}, failed: {} (see old-drivers.log)", c & 255, c >> 8 & 255),
            Some(c) => tf!("Удалено старых драйверов: {} ({})", "Old drivers removed: {} ({})", c & 255, crate::power::size(before.saturating_sub(after))),
        });
    });
}

/// The window's switch, in a thread; `done` gets the message.
pub fn set_auto(on: bool, done: impl FnOnce(String) + Send + 'static) {
    std::thread::spawn(move || {
        let what = if on { "auto-on" } else { "auto-off" };
        let code = if elevated() { Some(auto_code(on)) } else { run_elevated(what) };
        AUTO.store(match task_exists() { Some(true) => 2, Some(false) => 1, None => 0 }, Relaxed);
        done(match code {
            None => t!("Отменено: нужны права администратора", "Cancelled: needs administrator rights").into(),
            Some(0) if on => t!("Старые драйверы будут удаляться после установки нового", "Old drivers will be removed after a new one is installed").into(),
            Some(0) => t!("Автоудаление выключено", "Automatic removal turned off").into(),
            Some(2) => t!("Только из установленной копии (Program Files)", "Only from the installed copy (Program Files)").into(),
            Some(_) => t!("Не удалось изменить задание планировщика", "Could not change the scheduled task").into(),
        });
    });
}

/// `rust-settings --old-drivers clean|auto-on|auto-off` (elevated, or the task as SYSTEM). Returns the exit code.
pub fn command(what: &str) -> u32 {
    match what {
        "clean" => clean_code(),
        "auto-on" => auto_code(true),
        "auto-off" => auto_code(false),
        _ => 1,
    }
}

/// 0 done, 1 failed, 2 not the installed copy.
fn auto_code(on: bool) -> u32 {
    if !on {
        return if task_delete() { 0 } else { 1 };
    }
    let Some(exe) = installed_exe() else { return 2 };
    if task_register(&exe) { 0 } else { 1 }
}

// ---- the scheduled task (Task Scheduler COM, documented) ----

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The task, as SYSTEM: 10 minutes after a driver is installed, and 5 minutes after startup (in case the PC went off before that).
/// "Installed" = UserPnp event 20003 in the System log ("Driver Management has concluded the process to add Service ..."): a
/// display driver install writes it (twice: the driver and NVIDIA's container service), and little else does. The delay lets the
/// installer finish (it sets up the HD audio and USB-C devices after the display); a second event while waiting is ignored.
/// Authenticated users may read it, so the window shows the switch without administrator rights.
fn task_xml(exe: &std::path::Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><Author>WinRust</Author><Description>Removes display driver packages that no device uses and that are not the newest one (rust-settings, Power &amp; storage). Log: %ProgramData%\WinRust\old-drivers.log</Description><SecurityDescriptor>D:(A;;FA;;;BA)(A;;FA;;;SY)(A;;FR;;;AU)</SecurityDescriptor></RegistrationInfo>
  <Triggers>
    <EventTrigger><Delay>PT10M</Delay><Subscription>&lt;QueryList&gt;&lt;Query Id="0" Path="System"&gt;&lt;Select Path="System"&gt;*[System[Provider[@Name='Microsoft-Windows-UserPnp'] and EventID=20003]]&lt;/Select&gt;&lt;/Query&gt;&lt;/QueryList&gt;</Subscription></EventTrigger>
    <BootTrigger><Delay>PT5M</Delay></BootTrigger>
  </Triggers>
  <Principals><Principal id="Author"><UserId>S-1-5-18</UserId><RunLevel>HighestAvailable</RunLevel></Principal></Principals>
  <Settings><StartWhenAvailable>true</StartWhenAvailable><DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries><StopIfGoingOnBatteries>false</StopIfGoingOnBatteries><ExecutionTimeLimit>PT15M</ExecutionTimeLimit><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy><Priority>7</Priority></Settings>
  <Actions Context="Author"><Exec><Command>{}</Command><Arguments>--old-drivers clean</Arguments></Exec></Actions>
</Task>"#,
        esc(&exe.to_string_lossy())
    )
}

fn root() -> windows::core::Result<windows::Win32::System::TaskScheduler::ITaskFolder> {
    use windows::Win32::System::{
        Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx},
        TaskScheduler::{ITaskService, TaskScheduler},
        Variant::VARIANT,
    };
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED); // this thread; already done is fine
        let s: ITaskService = CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER)?;
        let e = VARIANT::default();
        s.Connect(&e, &e, &e, &e)?;
        s.GetFolder(&windows::core::BSTR::from(r"\"))
    }
}

/// Some(true/false), None: could not ask.
fn task_exists() -> Option<bool> {
    let f = root().ok()?;
    match unsafe { f.GetTask(&windows::core::BSTR::from(TASK)) } {
        Ok(_) => Some(true),
        Err(e) if e.code().0 as u32 == 0x8007_0002 => Some(false), // file not found
        Err(_) => None,
    }
}

fn task_register(exe: &std::path::Path) -> bool {
    use windows::Win32::System::{TaskScheduler::*, Variant::VARIANT};
    let ok = root().and_then(|f| unsafe {
        f.RegisterTask(&TASK.into(), &task_xml(exe).into(), TASK_CREATE_OR_UPDATE.0, &VARIANT::from("SYSTEM"), &VARIANT::default(), TASK_LOGON_SERVICE_ACCOUNT, &VARIANT::default())
    });
    if let Err(e) = &ok {
        log(&format!("task not registered: {e}"));
    }
    ok.is_ok()
}

fn task_delete() -> bool {
    match root().and_then(|f| unsafe { f.DeleteTask(&TASK.into(), 0) }) {
        Ok(()) => true,
        Err(e) => e.code().0 as u32 == 0x8007_0002, // already gone
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkg(inf: &str, provider: &str, version: &str) -> Pkg {
        Pkg { inf: inf.into(), provider: provider.into(), version: version.into(), bytes: 1, old: false }
    }

    #[test]
    fn only_unused_older_packages_count_as_old() {
        let mut p = vec![
            pkg("oem23.inf", "NVIDIA", "32.0.16.2135"), // in use, newest
            pkg("oem72.inf", "NVIDIA", "32.0.16.2124"), // older, unused
            pkg("oem9.inf", "NVIDIA", "32.0.16.900"),   // older but a device (an unplugged card) still has it
            pkg("oem5.inf", "Intel", "31.0.101.1"),     // the only one of its provider
        ];
        mark_old(&mut p, &["oem23.inf".into(), "oem9.inf".into()]);
        assert_eq!(p.iter().map(|p| p.old).collect::<Vec<_>>(), [false, true, false, false]);
    }

    #[test]
    fn the_newest_stays_even_when_nothing_uses_it_yet() {
        // a driver just staged by an installer, before the device switches to it
        let mut p = vec![pkg("oem1.inf", "NVIDIA", "32.0.16.2135"), pkg("oem2.inf", "NVIDIA", "32.0.16.2124")];
        mark_old(&mut p, &["oem2.inf".into()]);
        assert_eq!(p.iter().map(|p| p.old).collect::<Vec<_>>(), [false, false]);
    }

    /// Reads this PC (nothing is changed): `cargo test -p rust-settings print_driver_scan -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn print_driver_scan() {
        for p in scan() {
            println!("{p:?}");
        }
        println!("task exists: {:?}, elevated: {}", task_exists(), elevated());
    }

    #[test]
    fn versions_compare_as_numbers() {
        assert!(version_key("32.0.16.10") > version_key("32.0.16.9"));
    }

    #[test]
    fn the_task_runs_the_exe_with_the_clean_argument() {
        let x = task_xml(std::path::Path::new(r"C:\Program Files\rust-settings\rust-settings.exe"));
        assert!(x.contains(r"<Command>C:\Program Files\rust-settings\rust-settings.exe</Command>"));
        assert!(x.contains("<Arguments>--old-drivers clean</Arguments>"));
        assert!(x.contains("<UserId>S-1-5-18</UserId>"));
        assert!(x.contains("Microsoft-Windows-UserPnp'] and EventID=20003") && !x.contains("CalendarTrigger"));
    }
}
