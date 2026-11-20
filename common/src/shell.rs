//! Starting Windows Explorer as the shell again (after rs-shell, or for the time Windows Settings is open).
//!
//! It must run with the signed-in user's normal token. `runas /trustlevel:0x20000 explorer.exe` was used for that, but
//! started from an elevated process (the installer, an elevated bar) it gives a token of the elevated *type*
//! (TokenElevationType 2, only trimmed by the Basic User rules): Explorer then counts as elevated and Store apps started
//! through it (Calculator...) open suspended and hidden. A one-shot scheduled task with RunLevel Limited for the
//! interactive user gets the normal sign-in token (TokenElevationType 3) whoever starts it, admin or not. The task is
//! removed right after it started; Explorer keeps running. If the task cannot be made, runas is the fallback.
use std::{os::windows::process::CommandExt, process::{Command, Stdio}};

/// PowerShell that starts Explorer as described above (paste into a script, or run with `start_explorer`).
pub const START_EXPLORER_PS: &str = r#"
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
