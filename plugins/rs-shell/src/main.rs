// rs-shell              run as the user's shell: wallpaper, Run keys + Startup folders, topbar (restarted when it dies),
//                       explorer.exe as the fallback shell if topbar keeps dying
// rs-shell --enable     make it THIS user's shell (HKCU Winlogon\Shell; other users and the system are untouched)
// rs-shell --disable    back to the normal shell (deletes that one value)
// rs-shell --dry-run    print what it would start, change nothing
// rs-shell --overlay    run NEXT TO explorer.exe (it stays the shell): topbar + desktop + keep the bar alive. Started at logon by the "rs-overlay" scheduled task (HKCU Run as the fallback).
// Put it next to topbar.exe. Recovery from a black screen: Ctrl+Alt+Del -> Task Manager -> run explorer.exe,
// then `rs-shell --disable`; or sign in as another admin and run `reg delete` on the key printed by --enable.
// Also: starts `rust-explorer.exe --desktop` (desktop icons), Win+D = show desktop / bring the windows back (winrust-common),
// Win+E = file manager (rust-explorer.exe if next to us). Put rust-explorer.exe next to rs-shell.exe.
// shortcuts: Win+D only minimizes ordinary top-level windows (no per-monitor / virtual-desktop logic),
// Run/Startup entries are started through `cmd /c start` (no RunOnce, no startup-delay rules, no disabled-list in
// StartupApproved), topbar's "open" still shells out to explorer.exe for /select; upgrade when those matter.
use std::{os::windows::process::CommandExt, path::PathBuf, process::Command, ptr, thread, time::{Duration, Instant}};
use windows_sys::{core::BOOL, Win32::{Foundation::{HWND, LPARAM, RECT}, UI::{Input::KeyboardAndMouse::{MOD_NOREPEAT, MOD_WIN, RegisterHotKey}, WindowsAndMessaging::*}}};

const WINLOGON: &str = r"HKCU\Software\Microsoft\Windows NT\CurrentVersion\Winlogon";
const NO_WINDOW: u32 = 0x0800_0000;

/// A Command with null std handles. After FreeConsole() the shell has no valid ones, and spawn() inheriting
/// them fails with "The handle is invalid" (this broke every start the first time).
fn quiet(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut c = Command::new(program);
    c.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    c
}

fn reg(args: &[&str]) -> Option<String> {
    let o = Command::new("reg").args(args).creation_flags(NO_WINDOW).output().ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).into_owned())
}

/// Commands from `reg query <Run key>` output: "    Name    REG_SZ    command line".
fn parse_run(out: &str) -> Vec<String> {
    out.lines().filter_map(|l| {
        let (_, v) = l.split_once("REG_EXPAND_SZ").or_else(|| l.split_once("REG_SZ"))?;
        Some(v.trim().to_string()).filter(|v| !v.is_empty())
    }).collect()
}

/// The exe a Run command line points at, %VARS% expanded; None when it is not an .exe we can tell apart (scripts, URIs).
fn exe_path(cmd: &str) -> Option<String> {
    let mut c = cmd.trim().to_string();
    while let Some(a) = c.find('%') {
        let b = c[a + 1..].find('%')?;
        let val = std::env::var(&c[a + 1..a + 1 + b]).ok()?;
        c = format!("{}{}{}", &c[..a], val, &c[a + b + 2..]);
    }
    let path = match c.strip_prefix('"') {
        Some(rest) => rest.split('"').next()?.to_string(),
        None => c[..c.to_lowercase().find(".exe")? + 4].to_string(),
    };
    path.to_lowercase().ends_with(".exe").then_some(path)
}

/// Everything the shell should start at logon (commands for `cmd /c start ""`).
fn autostart() -> Vec<String> {
    let mut v = vec![];
    for key in [r"HKLM\Software\Microsoft\Windows\CurrentVersion\Run", r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run"] {
        v.extend(reg(&["query", key]).map(|o| parse_run(&o)).unwrap_or_default());
    }
    let dirs = [std::env::var("APPDATA").map(|a| PathBuf::from(a).join(r"Microsoft\Windows\Start Menu\Programs\Startup")), std::env::var("ProgramData").map(|a| PathBuf::from(a).join(r"Microsoft\Windows\Start Menu\Programs\StartUp"))];
    for d in dirs.into_iter().flatten() {
        for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
            if e.file_name().to_string_lossy().eq_ignore_ascii_case("desktop.ini") { continue; }
            v.push(format!("\"{}\"", e.path().display()));
        }
    }
    v.retain(|c| !c.to_lowercase().contains("rs-shell")); // never start ourselves twice
    // topbar's own helper (reshell) is started from our folder below, not from the Run entry that points into Program Files
    v.retain(|c| !c.to_lowercase().contains(r"\topbar\"));
    // a Run entry whose exe is gone is skipped silently by explorer; `cmd /c start` would pop an error dialog (Rave did)
    v.retain(|c| exe_path(c).is_none_or(|p| std::path::Path::new(&p).exists()));
    v
}

fn wide(s: &str) -> Vec<u16> { s.encode_utf16().chain([0]).collect() }

fn dir() -> PathBuf { std::env::current_exe().ok().and_then(|e| e.parent().map(|p| p.to_path_buf())).unwrap_or_default() }

/// An exe next to us, else the one the topbar plugin installed (the rust-explorer plugin keeps only its own files in its folder).
fn find_exe(name: &str) -> PathBuf {
    let own = dir().join(name);
    if own.exists() { return own; }
    PathBuf::from(std::env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".into())).join("topbar").join(name)
}

fn bar_up() -> bool { unsafe { !FindWindowW(wide("topbar").as_ptr(), std::ptr::null()).is_null() } }

/// A minimized window left on screen (or minimized before the hide setting was applied) shows as a caption stub on
/// the desktop: park it where Windows itself parks hidden minimized windows.
unsafe extern "system" fn park(h: HWND, _: LPARAM) -> BOOL {
    unsafe {
        if IsWindowVisible(h) != 0 && IsIconic(h) != 0 {
            let mut r: RECT = std::mem::zeroed();
            GetWindowRect(h, &mut r);
            if r.left > -30000 { SetWindowPos(h, ptr::null_mut(), -32000, -32000, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE); }
        }
        1
    }
}

fn set_wallpaper() {
    let Some(src) = reg(&["query", r"HKCU\Control Panel\Desktop", "/v", "WallPaper"]).map(|o| parse_run(&o)).and_then(|v| v.into_iter().next()) else { return };
    let mut path = PathBuf::from(&src);
    // without explorer's image transcoder the desktop paints only BMP: convert once (System.Drawing) and keep the copy
    if !src.to_lowercase().ends_with(".bmp") {
        let Ok(base) = std::env::var("LOCALAPPDATA") else { return };
        let bmp = PathBuf::from(base).join("rs-shell").join("wallpaper.bmp");
        let modified = |p: &std::path::Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
        if modified(&bmp) < modified(&PathBuf::from(&src)) {
            let _ = std::fs::create_dir_all(bmp.parent().unwrap_or(&bmp));
            let script = format!("Add-Type -AssemblyName System.Drawing; $i=[Drawing.Image]::FromFile('{}'); $i.Save('{}',[Drawing.Imaging.ImageFormat]::Bmp)", src.replace('\'', "''"), bmp.display().to_string().replace('\'', "''"));
            let _ = quiet("powershell").args(["-NoProfile", "-NonInteractive", "-Command", &script]).creation_flags(NO_WINDOW).status();
        }
        path = bmp;
    }
    unsafe { SystemParametersInfoW(0x14, 0, wide(&path.to_string_lossy()).as_ptr() as _, 0) }; // SPI_SETDESKWALLPAPER
}

/// Appends a line to %LOCALAPPDATA%\rs-shell\log.txt: a shell has no console, so this is the only way to see what it did.
fn log(msg: &str) {
    use std::io::Write;
    let Ok(base) = std::env::var("LOCALAPPDATA") else { return };
    let dir = PathBuf::from(base).join("rs-shell");
    let _ = std::fs::create_dir_all(&dir);
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("log.txt")) { let _ = writeln!(f, "[{t}] {msg}"); }
}

fn run_shell() {
    unsafe { windows_sys::Win32::System::Console::FreeConsole() };
    log("start");
    // without explorer a minimized window is drawn as a little caption stub on the desktop (apps started with
    // --minimized left grey rectangles over the icons): hide them, the bar lists the windows anyway
    let mut mm = MINIMIZEDMETRICS { cbSize: size_of::<MINIMIZEDMETRICS>() as u32, iWidth: 0, iHorzGap: 0, iVertGap: 0, iArrange: ARW_HIDE as i32 };
    unsafe { SystemParametersInfoW(SPI_SETMINIMIZEDMETRICS, mm.cbSize, &mut mm as *mut _ as _, 0) };
    // the desktop is blank without explorer: repaint the user's wallpaper (again below, once the session has settled)
    thread::spawn(set_wallpaper); // the first conversion takes a second or two; do not hold up autostart
    let list = autostart();
    log(&format!("autostart: {} entries", list.len()));
    for c in list {
        let r = quiet("cmd").raw_arg(format!("/c start \"\" {c}")).creation_flags(NO_WINDOW).spawn();
        log(&format!("start {c} -> {:?}", r.map(|_| ())));
    }
    thread::sleep(Duration::from_secs(4)); // topbar may already have been started by its own Run entry
    set_wallpaper();
    // no bar yet (its logon task is gone in user mode): start it and its hotkey helper from our folder, with our normal rights
    if !bar_up() { log(&format!("topbar spawn -> {:?}", quiet(find_exe("topbar.exe")).spawn().map(|_| ()))); }
    let hotkeys = find_exe("reshell.exe"); // lives in topbar's folder, not ours
    if hotkeys.exists() { log(&format!("reshell spawn -> {:?}", quiet(&hotkeys).spawn().map(|_| ()))); }
    let desktop = dir().join("rust-explorer.exe");
    // the rust-explorer desktop: class "Progman" (so Wallpaper Engine finds it), title "rs-desktop" (explorer's is "Program Manager")
    let desktop_up = || unsafe { !FindWindowW(wide("Progman").as_ptr(), wide("rs-desktop").as_ptr()).is_null() };
    log(&format!("desktop exe {} exists={} up={}", desktop.display(), desktop.exists(), desktop_up()));
    if desktop.exists() && !desktop_up() { log(&format!("desktop spawn -> {:?}", quiet(&desktop).arg("--desktop").spawn().map(|_| ()))); }
    // the supervisor runs on its own thread; this one owns the hotkeys (RegisterHotKey needs the thread's message loop)
    thread::spawn(supervise);
    hotkey_loop();
}

/// Win+D (show desktop) and Win+E (file manager) for sessions where Explorer is not the shell. Never returns.
fn hotkey_loop() {
    unsafe {
        RegisterHotKey(ptr::null_mut(), 1, MOD_WIN | MOD_NOREPEAT, 'D' as u32);
        RegisterHotKey(ptr::null_mut(), 2, MOD_WIN | MOD_NOREPEAT, 'E' as u32);
        let mut m: MSG = std::mem::zeroed();
        while GetMessageW(&mut m, ptr::null_mut(), 0, 0) > 0 {
            match (m.message == WM_HOTKEY).then_some(m.wParam) {
                Some(1) => winrust_common::desktop::toggle(),
                Some(2) => { let fm = dir().join("rust-explorer.exe"); let _ = quiet(if fm.exists() { fm } else { "explorer.exe".into() }).spawn(); }
                _ => {}
            }
        }
    }
}

/// Keeps topbar alive; if it will not stay up, hands the session to explorer.exe and ends the shell.
fn supervise() {
    let (mut starts, mut since, mut misses) = (0, Instant::now(), 0);
    loop {
        unsafe { EnumWindows(Some(park), 0) };
        if bar_up() { misses = 0; } else { misses += 1; }
        if misses >= 6 { // ~6 s: the loop ticks every second now
            if since.elapsed() > Duration::from_secs(120) { (starts, since) = (0, Instant::now()); }
            starts += 1;
            if starts > 3 || quiet(find_exe("topbar.exe")).spawn().is_err() {
                // topbar will not stay up: hand the session to explorer so the user is never left with nothing
                let _ = quiet(find_exe("topbar.exe")).arg("--restore").creation_flags(NO_WINDOW).status();
                let _ = quiet("explorer.exe").spawn();
                std::process::exit(0);
            }
            misses = 0;
        }
        thread::sleep(Duration::from_secs(1));
    }
}

/// `rs-shell --overlay`: runs next to explorer.exe, which stays the real shell (Win+D / Win+E / wallpaper / Run keys are
/// its job). Starts topbar and the rust-explorer desktop on top of it and keeps the bar alive. Started from the user's
/// Run key, so it can live in a folder the user owns (Windows stopped starting our own shell at logon).
/// The rust-explorer plugin's "replace Explorer" switch (HKCU\Software\rs-shell\ReplaceExplorer = 1).
fn replace_on() -> bool {
    reg(&["query", r"HKCU\Software\rs-shell", "/v", "ReplaceExplorer"]).is_some_and(|o| o.contains("0x1"))
}

/// Ends the Explorer that runs as the shell (started without arguments). Folder windows (started with a path) and the COM
/// host (`/factory ... -Embedding`) are other explorer.exe processes and keep running.
fn kill_shell_explorer() {
    let script = r#"Get-CimInstance Win32_Process -Filter "Name='explorer.exe'" | Where-Object { $_.CommandLine -match '(?i)explorer\.exe"?(\s+/NoUACCheck)?\s*$' } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force }"#;
    let r = quiet("powershell").args(["-NoProfile", "-NonInteractive", "-Command", script]).creation_flags(NO_WINDOW).status();
    log(&format!("explorer shell stopped -> {:?}", r.map(|s| s.success())));
}

/// How long after the overlay starts Explorer may be ended even without the markers below: newer Windows builds (26300)
/// no longer write them, and the overlay then waited forever. Explorer gets through the autostart well within this.
const AUTOSTART_GRACE: Duration = Duration::from_secs(90);

/// True once Explorer has started this session's Run keys and Startup folder. The overlay starts at logon (a scheduled task),
/// before Explorer gets to them: ending Explorer earlier would lose the user's autostart apps.
fn startup_run() -> bool {
    let mut sid = 0;
    unsafe { windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId(std::process::id(), &mut sid) };
    let k = format!(r"HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\SessionInfo\{sid}");
    ["RunStuffHasBeenRun", "StartupHasBeenRun"].iter().all(|n| reg(&["query", &format!(r"{k}\{n}")]).is_some())
}

fn may_end_explorer(markers: bool, since_start: Duration) -> bool {
    markers || since_start >= AUTOSTART_GRACE
}

/// Gives the user Explorer back (with a normal, not elevated, token: the shell must never run elevated; see
/// winrust_common::shell for why not runas /trustlevel).
fn start_shell_explorer() {
    let r = winrust_common::shell::start_explorer(false);
    log(&format!("explorer shell started -> {r:?}"));
}

fn overlay() {
    unsafe { windows_sys::Win32::System::Console::FreeConsole() };
    let replace = replace_on();
    log(&format!("overlay start (replace explorer: {replace})"));
    let wp = thread::spawn(set_wallpaper); // makes sure the BMP the desktop window paints exists (and refreshes the wallpaper)
    if !bar_up() { log(&format!("topbar spawn -> {:?}", quiet(find_exe("topbar.exe")).spawn().map(|_| ()))); }
    // Win+I / Win+E / Win+D live in reshell: its Run entry may be missing (another account's HKCU), and without it Explorer's own Win+I
    // fails once the shell is gone. A second copy exits at once (one hook per session).
    let hk = find_exe("reshell.exe");
    if hk.exists() { log(&format!("reshell spawn -> {:?}", quiet(&hk).spawn().map(|_| ()))); }
    let _ = wp.join(); // the BMP conversion (only on a fresh install or a new wallpaper) finishes before the desktop paints it
    let desktop = dir().join("rust-explorer.exe");
    let up = || unsafe { !FindWindowW(wide("Progman").as_ptr(), wide("rs-desktop").as_ptr()).is_null() };
    if desktop.exists() && !up() { log(&format!("desktop spawn -> {:?}", quiet(&desktop).arg("--desktop").spawn().map(|_| ()))); }
    if replace {
        // Explorer is being replaced: this thread watches, the main one owns Win+D / Win+E
        thread::spawn(move || overlay_watch(replace, desktop, up));
        hotkey_loop();
    } else {
        overlay_watch(replace, desktop, up);
    }
}

/// Keeps topbar and the desktop alive. With `replace` it also ends the shell Explorer once both are up, and gives it back
/// (so the user is never left without a shell) if the bar will not stay up.
fn overlay_watch(replace: bool, desktop: PathBuf, up: impl Fn() -> bool) {
    let (mut starts, mut misses, mut dmiss, mut stable, mut killed) = (0, 0, 0, 0, false);
    let began = Instant::now();
    loop {
        thread::sleep(Duration::from_secs(3));
        if replace && !killed {
            // both our surfaces must be on screen before the system's one goes away
            stable = if bar_up() && up() { stable + 1 } else { 0 };
            if stable >= 3 && may_end_explorer(startup_run(), began.elapsed()) {
                log(&format!("autostart done (markers: {}, after {} s)", startup_run(), began.elapsed().as_secs()));
                kill_shell_explorer();
                killed = true;
            }
        }
        dmiss = if desktop.exists() && !up() { dmiss + 1 } else { 0 };
        if dmiss >= 2 { log(&format!("desktop respawn -> {:?}", quiet(&desktop).arg("--desktop").spawn().map(|_| ()))); dmiss = 0; }
        if bar_up() { misses = 0; continue; }
        misses += 1;
        if misses < 2 { continue; }
        starts += 1;
        if starts > 3 || quiet(find_exe("topbar.exe")).spawn().is_err() {
            // the bar will not stay up: give the system's taskbar (and, if we ended it, Explorer) back and stop
            let _ = quiet(find_exe("topbar.exe")).arg("--restore").creation_flags(NO_WINDOW).status();
            if replace { start_shell_explorer(); }
            log("overlay: topbar keeps dying, system shell restored");
            return;
        }
        misses = 0;
    }
}

/// Windows will not start a shell from a folder the user can write to (a boot from %LOCALAPPDATA% fell back to explorer),
/// so the copy in Program Files is only a loader: it runs the real rs-shell from %LOCALAPPDATA%\plugin-launcher\bin, which
/// can be updated without admin. true = the user-folder shell took over (the loader must not run the shell logic as well).
/// A missing copy or one that dies within 3 s (it started nothing yet) leaves false: the loader then does the job itself.
fn bootstrap() -> bool {
    let Ok(base) = std::env::var("LOCALAPPDATA") else { return false };
    let real = PathBuf::from(base).join(r"plugin-launcher\bin\rs-shell.exe");
    let me = std::env::current_exe().and_then(|p| p.canonicalize()).ok();
    if !real.exists() || real.canonicalize().ok() == me {
        return false;
    }
    let t = Instant::now();
    match quiet(&real).spawn() {
        Ok(mut child) => {
            log(&format!("loader: started {}", real.display()));
            let _ = child.wait();
            t.elapsed() > Duration::from_secs(3)
        }
        Err(e) => {
            log(&format!("loader: cannot start {}: {e}", real.display()));
            false
        }
    }
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let code = match a.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] => { if !bootstrap() { run_shell(); } 0 }
        ["--overlay"] => { overlay(); 0 }
        ["--dry-run"] => {
            println!("wallpaper: {:?}", reg(&["query", r"HKCU\Control Panel\Desktop", "/v", "WallPaper"]).map(|o| parse_run(&o)));
            autostart().iter().for_each(|c| println!("autostart: {c}"));
            println!("topbar: {} ({})", find_exe("topbar.exe").display(), if find_exe("topbar.exe").exists() { "found" } else { "MISSING" });
            println!("explorer ran this session's autostart: {}", startup_run());
            0
        }
        ["--enable"] => {
            let exe = std::env::current_exe().unwrap_or_default();
            if !find_exe("topbar.exe").exists() {
                eprintln!("refusing: topbar.exe is not next to {} (no bar = no way to launch anything)", exe.display());
                1
            } else if reg(&["add", WINLOGON, "/v", "Shell", "/t", "REG_SZ", "/d", &exe.to_string_lossy(), "/f"]).is_some() {
                println!("done: you get rs-shell instead of explorer at the next sign-in.\nundo: rs-shell --disable   (or: reg delete \"{WINLOGON}\" /v Shell /f)");
                0
            } else { eprintln!("reg add failed"); 1 }
        }
        ["--disable"] => {
            // deleting a value that is not there is fine too
            let _ = reg(&["delete", WINLOGON, "/v", "Shell", "/f"]);
            println!("done: normal shell at the next sign-in");
            0
        }
        _ => { eprintln!("usage: rs-shell [--enable | --disable | --dry-run]"); 2 }
    };
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::{exe_path, may_end_explorer, parse_run};
    use std::time::Duration;
    #[test]
    fn ends_explorer_on_markers_or_after_grace() {
        assert!(may_end_explorer(true, Duration::ZERO));
        assert!(!may_end_explorer(false, Duration::from_secs(30)));
        assert!(may_end_explorer(false, Duration::from_secs(90)));
    }
    #[test]
    fn finds_the_exe_in_run_commands() {
        assert_eq!(exe_path(r#""C:\a b\x.exe" --hidden"#).as_deref(), Some(r"C:\a b\x.exe"));
        assert_eq!(exe_path(r"C:\d\y.exe /background").as_deref(), Some(r"C:\d\y.exe"));
        assert_eq!(exe_path("script.bat"), None);
        assert!(exe_path(r"%windir%\system32\z.exe").unwrap().to_lowercase().ends_with(r"system32\z.exe"));
    }
    #[test]
    fn parses_reg_query_lines() {
        let out = "\nHKEY_CURRENT_USER\\X\n    Foo    REG_SZ    foo.exe --x\n    Bar    REG_EXPAND_SZ    %APPDATA%\\bar.exe\n    Empty    REG_SZ    \n";
        assert_eq!(parse_run(out), ["foo.exe --x", "%APPDATA%\\bar.exe"]);
    }
}
