//! Starting Windows Explorer as the shell again (after rs-shell, or for the time Windows Settings is open).
//!
//! It must run with the signed-in user's normal token. `runas /trustlevel:0x20000 explorer.exe` was used for that, but
//! started from an elevated process (the installer, an elevated bar) it gives a token of the elevated *type*
//! (TokenElevationType 2, only trimmed by the Basic User rules): Explorer then counts as elevated and Store apps started
//! through it (Calculator...) open suspended and hidden. A one-shot scheduled task with RunLevel Limited for the
//! interactive user gets the normal sign-in token (TokenElevationType 3) whoever starts it, admin or not. The task is
//! removed right after it started; Explorer keeps running. If the task cannot be made, runas is the fallback.
//!
//! The topbar, started while Explorer was away, is the desktop's "taskman" window (SetTaskmanWindow: minimized windows
//! need one). Explorer only takes that role at its start if it is free; without it Explorer's app-view host never shows a
//! Store app: Settings, Calculator... stay suspended on their splash screen. So the role is freed right before the start
//! (the bar takes it back when Explorer leaves).
//!
//! Without Explorer, Store apps are hosted by `ShellAppRuntime.exe` instead (see `ensure_app_host`), so Explorer is only
//! started again to give it back for good (rs-shell, the uninstaller). That host is ended first: it is the shell window
//! then, and Explorer would not become the shell next to it.
use std::{os::windows::process::CommandExt, path::PathBuf, process::{Command, Stdio}, time::{Duration, Instant}};
use windows_sys::Win32::UI::WindowsAndMessaging::GetShellWindow;

/// PowerShell that starts Explorer as described above (paste into a script, or run with `start_explorer`).
pub const START_EXPLORER_PS: &str = r#"
Stop-Process -Name ShellAppRuntime -Force -ErrorAction SilentlyContinue
Add-Type -Namespace WR -Name Tm -MemberDefinition '[System.Runtime.InteropServices.DllImport("user32.dll")] public static extern System.IntPtr GetTaskmanWindow(); [System.Runtime.InteropServices.DllImport("user32.dll")] public static extern bool SetTaskmanWindow(System.IntPtr h); [System.Runtime.InteropServices.DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(System.IntPtr h, out uint p);'
$tm=[WR.Tm]::GetTaskmanWindow(); $tp=0
if($tm -ne [IntPtr]::Zero -and [WR.Tm]::GetWindowThreadProcessId($tm,[ref]$tp) -and (Get-Process -Id $tp -ErrorAction SilentlyContinue).Name -ne 'explorer'){ [void][WR.Tm]::SetTaskmanWindow([IntPtr]::Zero) }
$u=(Get-CimInstance Win32_ComputerSystem).UserName; if(-not $u){ $u=[Security.Principal.WindowsIdentity]::GetCurrent().Name }
$n='winrust-explorer-'+[guid]::NewGuid().ToString('N').Substring(0,8)
try {
  Register-ScheduledTask -TaskName $n -Force -ErrorAction Stop -Action (New-ScheduledTaskAction -Execute "$env:SystemRoot\explorer.exe") -Principal (New-ScheduledTaskPrincipal -UserId $u -LogonType Interactive -RunLevel Limited) -Settings (New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit ([TimeSpan]::Zero)) | Out-Null
  Start-ScheduledTask -TaskName $n -ErrorAction Stop
  Start-Sleep -Seconds 2
  Unregister-ScheduledTask -TaskName $n -Confirm:$false -ErrorAction SilentlyContinue
} catch {
  Unregister-ScheduledTask -TaskName $n -Confirm:$false -ErrorAction SilentlyContinue
  & runas.exe /trustlevel:0x20000 "$env:SystemRoot\explorer.exe"
}
"#;

/// Starts Explorer with the user's normal token (see the module comment). `wait`: until the start was handed over.
pub fn start_explorer(wait: bool) -> std::io::Result<()> {
    let mut c = Command::new("powershell");
    c.args(["-NoProfile", "-NonInteractive", "-WindowStyle", "Hidden", "-Command", START_EXPLORER_PS])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    if wait { c.status().map(|_| ()) } else { c.spawn().map(|_| ()) }
}

/// Windows' own small host for Store (UWP) app windows: `ShellAppRuntime.exe`, built from the kiosk shell host
/// (CustomShellHost). A Store app's window (Settings, Calculator...) is only shown by a process that runs the app-view
/// host (twinui) and is the shell window: Explorer, or this. Without either, the app starts but stays cloaked and suspended
/// on its splash screen. The host makes the shell window (its desktop is the bottom-most window, under rust-explorer's)
/// and the taskman window, has no taskbar, and is ready ~0.2 s after its start (measured: a page of Settings on screen
/// 0.9 s after starting both, 0.6 s with the host already up; with Explorer started for it 5 s and a grey screen).
fn app_host_exe() -> Option<PathBuf> {
    let exe = PathBuf::from(std::env::var_os("SystemRoot")?).join(r"System32\ShellAppRuntime.exe");
    exe.is_file().then_some(exe)
}

/// A shell window exists: Explorer or the Store app host is up, so Store apps can show.
pub fn app_host_up() -> bool {
    unsafe { !GetShellWindow().is_null() }
}

/// Makes sure Store apps can show: with no shell window, starts `ShellAppRuntime.exe` and waits (up to 5 s) for it.
/// Started through explorer.exe, which hands it to its COM host: the user's normal token also from the elevated bar.
/// false = no host (no such exe on this Windows, or it did not come up). Blocks: call it off any UI thread.
pub fn ensure_app_host() -> bool {
    if app_host_up() {
        return true;
    }
    let Some(exe) = app_host_exe() else { return false };
    let explorer = PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into())).join("explorer.exe");
    if Command::new(explorer).arg(exe).stdin(Stdio::null()).spawn().is_err() {
        return false;
    }
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(5) {
        if app_host_up() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}
