//! Opens a Settings page (`ms-settings:...`). Settings is a Store app: it opens only while Explorer is the shell (activation
//! fails with 0x80040900 otherwise, also with a shell window of our own registered). So with rust-explorer in place of Explorer:
//! start Explorer for as long as Settings is open, then end it again, which keeps the RAM saving. Done by a hidden PowerShell:
//! it waits for processes, a hook or UI thread must not. Shared by Win+I (reshell) and the topbar's menus.
use std::{os::windows::process::CommandExt, process::{Command, Stdio}};

const PS: &str = r#"
Add-Type -Namespace W -Name U -MemberDefinition '[System.Runtime.InteropServices.DllImport("user32.dll")] public static extern System.IntPtr GetShellWindow(); [System.Runtime.InteropServices.DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(System.IntPtr h, out uint p); [System.Runtime.InteropServices.DllImport("user32.dll")] public static extern System.IntPtr FindWindowEx(System.IntPtr p, System.IntPtr a, string c, string t); [System.Runtime.InteropServices.DllImport("user32.dll")] public static extern System.IntPtr GetWindow(System.IntPtr h, uint c); [System.Runtime.InteropServices.DllImport("user32.dll")] public static extern bool SetForegroundWindow(System.IntPtr h); [System.Runtime.InteropServices.DllImport("user32.dll")] public static extern bool ShowWindow(System.IntPtr h, int c); [System.Runtime.InteropServices.DllImport("user32.dll")] public static extern void keybd_event(byte k, byte s, uint f, System.UIntPtr e); [System.Runtime.InteropServices.DllImport("user32.dll")] public static extern bool IsIconic(System.IntPtr h);'
function ExplorerShell { $h=[W.U]::GetShellWindow(); if($h -eq [IntPtr]::Zero){return $false}; $p=0; [void][W.U]::GetWindowThreadProcessId($h,[ref]$p); (Get-Process -Id $p -ErrorAction SilentlyContinue).Name -eq 'explorer' }
function ExplorerTray { $h=[W.U]::GetShellWindow(); if($h -eq [IntPtr]::Zero){return $false}; $sp=0; [void][W.U]::GetWindowThreadProcessId($h,[ref]$sp); $t=[IntPtr]::Zero
  while(($t=[W.U]::FindWindowEx([IntPtr]::Zero,$t,'Shell_TrayWnd',$null)) -ne [IntPtr]::Zero){ $tp=0; [void][W.U]::GetWindowThreadProcessId($t,[ref]$tp); if($tp -eq $sp){return $true} }
  $false }
# a fullscreen game keeps the screen: Settings opens behind it unless it is brought to the front
function Raise { $sp=(Get-Process SystemSettings -ErrorAction SilentlyContinue | Select-Object -First 1).Id; if(-not $sp){return}; $c=[IntPtr]::Zero
  while(($c=[W.U]::FindWindowEx([IntPtr]::Zero,$c,'Windows.UI.Core.CoreWindow',$null)) -ne [IntPtr]::Zero){ $cp=0; [void][W.U]::GetWindowThreadProcessId($c,[ref]$cp)
    if($cp -eq $sp){ $o=[W.U]::GetWindow($c,4); if($o -eq [IntPtr]::Zero){$o=$c}; [W.U]::keybd_event(0x12,0,0,[UIntPtr]::Zero); [W.U]::keybd_event(0x12,0,2,[UIntPtr]::Zero); [void][W.U]::ShowWindow($o,9); [void][W.U]::SetForegroundWindow($o); return } } }
# minimized, a Store app is suspended by Windows and stops answering: that is not a hang
function Minimized { $f=[IntPtr]::Zero; while(($f=[W.U]::FindWindowEx([IntPtr]::Zero,$f,'ApplicationFrameWindow','Settings')) -ne [IntPtr]::Zero){ if([W.U]::IsIconic($f)){ return $true } }; $false }
# a Settings process that hangs (opened before the shell was ready) swallows every later launch: it must go
function KillStale { Get-Process SystemSettings -ErrorAction SilentlyContinue | Where-Object { -not $_.Responding } | Stop-Process -Force }
KillStale
$started=$false
if(-not (ExplorerShell)){
  # our desktop is a window of class Progman too: with it up Explorer thinks a shell exists and starts as a plain folder host
  Get-Process rust-explorer -ErrorAction SilentlyContinue | ForEach-Object { if((Get-CimInstance Win32_Process -Filter "ProcessId=$($_.Id)").CommandLine -match '--desktop'){ Stop-Process -Id $_.Id -Force } }
  __START_EXPLORER__
  for($i=0;$i -lt 200 -and -not (ExplorerShell);$i++){ Start-Sleep -Milliseconds 100 }
  # the shell window appears before the immersive shell services behind it are up: Explorer's own taskbar comes last (the
  # topbar has a Shell_TrayWnd of its own, so only one that belongs to the shell process counts)
  for($i=0;$i -lt 150 -and -not (ExplorerTray);$i++){ Start-Sleep -Milliseconds 100 }
  Start-Sleep -Milliseconds 1000
  $started=$true
}
# through explorer.exe: the page opens with the user's normal token even when this process is elevated
for($try=0;$try -lt 2;$try++){
  & C:\Windows\explorer.exe $env:WR_URI
  for($i=0;$i -lt 80 -and -not (Get-Process SystemSettings -ErrorAction SilentlyContinue);$i++){ Start-Sleep -Milliseconds 100 }
  Start-Sleep -Seconds 1; Raise; Start-Sleep -Seconds 3; Raise
  if(-not (Get-Process SystemSettings -ErrorAction SilentlyContinue | Where-Object { -not $_.Responding })){ break }
  KillStale
}
if($started){
  # ends when Settings is closed, or when it hangs for good
  $bad=0
  while($bad -lt 5 -and (Get-Process SystemSettings -ErrorAction SilentlyContinue)){ Start-Sleep -Seconds 2; if((Get-Process SystemSettings -ErrorAction SilentlyContinue | Where-Object { -not $_.Responding }) -and -not (Minimized)){ $bad++ } else { $bad=0 } }
  KillStale
  Get-CimInstance Win32_Process -Filter "Name='explorer.exe'" | Where-Object { $_.CommandLine -match '(?i)explorer\.exe"?(\s+/NoUACCheck)?\s*$' } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force }
  $rx="$env:ProgramFiles\rust-explorer\rust-explorer.exe"
  if((Test-Path $rx) -and -not (Get-CimInstance Win32_Process -Filter "Name='rust-explorer.exe'" | Where-Object { $_.CommandLine -match '--desktop' })){ Start-Process $rx -ArgumentList '--desktop' }
}
"#;

/// The rust-settings page that stands for an exact `ms-settings:` address (a bare `ms-settings:` is Win+I: the first page).
/// Only these: any other address keeps going to Windows' own Settings.
fn own_page(uri: &str) -> Option<&'static str> {
    match uri.trim().to_ascii_lowercase().as_str() {
        "ms-settings:" | "ms-settings:display" | "ms-settings:nightlight" => Some("display"),
        "ms-settings:sound" => Some("sound"),
        "ms-settings:personalization" | "ms-settings:colors" | "ms-settings:personalization-colors" | "ms-settings:themes" => Some("personalization"),
        "ms-settings:powersleep" | "ms-settings:storagesense" | "ms-settings:batterysaver" => Some("power"),
        "ms-settings:network-status" | "ms-settings:network-wifi" | "ms-settings:network-wifisettings" | "ms-settings:network-vpn" | "ms-settings:network-proxy" => Some("network"),
        "ms-settings:bluetooth" | "ms-settings:network-airplanemode" => Some("bluetooth"),
        "ms-settings:appsfeatures" => Some("apps"),
        "ms-settings:windowsupdate" => Some("update"),
        "ms-settings:about" => Some("about"),
        _ => None,
    }
}

/// rust-settings.exe where the installer puts it: Program Files is writable by administrators only (a user-writable
/// folder would let any program swap the exe that Win+I starts).
fn own_exe() -> Option<std::path::PathBuf> {
    let exe = std::path::PathBuf::from(std::env::var_os("ProgramFiles")?).join("rust-settings").join("rust-settings.exe");
    exe.is_file().then_some(exe)
}

/// `uri` is passed through the environment, never spliced into the script. A second call while the first still waits is harmless.
pub fn open(uri: &str) {
    if let (Some(page), Some(exe)) = (own_page(uri), own_exe()) {
        if Command::new(exe).arg(page).stdin(Stdio::null()).creation_flags(0x0800_0000).spawn().is_ok() {
            return;
        }
    }
    open_windows(uri);
}

/// Always Windows' own Settings, also for the pages rust-settings has (the Start search: someone who looks up
/// "Installed apps" wants the full Windows page, with Store apps, sizes and app options).
pub fn open_windows(uri: &str) {
    // the Explorer start is shared (crate::shell: the normal user token, also from the elevated bar)
    let ps = PS.replace("__START_EXPLORER__", crate::shell::START_EXPLORER_PS);
    let _ = Command::new("powershell").args(["-NoProfile", "-NonInteractive", "-WindowStyle", "Hidden", "-Command", &ps])
        .env("WR_URI", uri).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).creation_flags(0x0800_0000).spawn();
}

#[cfg(test)]
mod tests {
    use super::own_page;

    #[test]
    fn only_exact_addresses_go_to_rust_settings() {
        assert_eq!(own_page("ms-settings:"), Some("display"));
        assert_eq!(own_page("MS-Settings:Sound"), Some("sound"));
        assert_eq!(own_page("ms-settings:themes"), Some("personalization"));
        assert_eq!(own_page("ms-settings:powersleep"), Some("power"));
        assert_eq!(own_page("ms-settings:network-status"), Some("network"));
        assert_eq!(own_page("ms-settings:personalization-background"), None);
        assert_eq!(own_page("ms-settings:network-wifi"), Some("network"));
        assert_eq!(own_page("ms-settings:bluetooth"), Some("bluetooth"));
        assert_eq!(own_page("ms-settings:windowsupdate"), Some("update"));
        assert_eq!(own_page("ms-settings:easeofaccess"), None);
        assert_eq!(own_page("ms-settings:sound; calc"), None);
    }
}
