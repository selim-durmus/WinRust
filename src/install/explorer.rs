//! rust-explorer: the file manager that replaces Explorer as folder handler and (with rs-shell) as the shell.
use crate::*;

pub(crate) fn rx_dir() -> PathBuf {
    program_files().join("rust-explorer")
}

// ---------- rust-explorer: file manager + Explorer replacement ----------
//
// Install: our files in Program Files, folders / drives opened from any program go to rust-explorer (HKCU class verbs of the
// installing user), rs-shell --overlay starts at sign-in and, with the ReplaceExplorer switch, ends the shell Explorer once topbar
// and the rust-explorer desktop are on screen. AutoRestartShell is set to 0 so Windows does not bring Explorer back.
// Uninstall puts everything back and starts Explorer again. rs-shell itself hands the session back to Explorer if topbar dies.
pub(crate) const RX_STATE: &str = r"HKLM\SOFTWARE\plugin-launcher\rust-explorer";
pub(crate) const WINLOGON_KEY: &str = r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon";
pub(crate) const RX_SWITCH: &str = r"HKCU\Software\rs-shell";
pub(crate) const RX_FOLDERS: [&str; 3] = [r"HKCU\Software\Classes\Directory\shell\open", r"HKCU\Software\Classes\Folder\shell\open", r"HKCU\Software\Classes\Drive\shell\open"];

// svchost RAM trim, done with rust-explorer: (service, Start we set, Windows default). Uninstall puts back the Windows defaults,
// not the values found at install: they may already be our own (the same tweak was first run by hand).
// DoSvc's key is ACL-protected on some builds, so every step is soft: a refused one is only a warning.
// DiagTrack (telemetry) is debloat's job: two plugins switching it would turn it back on when either one is removed.
pub(crate) const RX_SVCS: [(&str, u32, u32); 5] = [("PcaSvc", 4, 2), ("InventorySvc", 4, 2), ("lfsvc", 4, 3), ("cbdhsvc", 4, 2), ("DoSvc", 3, 2)];
pub(crate) const SPLIT_KEY: &str = r"HKLM\SYSTEM\CurrentControlSet\Control";
pub(crate) const SPLIT_DEFAULT: u32 = 3670016; // 3.5 GB: above it Windows gives every service its own svchost

pub(crate) fn rx_trim(on: bool) {
    for (s, off, def) in RX_SVCS {
        let v = if on { off } else { def };
        soft(&format!("service {s}"), run("reg", &["add", &format!(r"HKLM\SYSTEM\CurrentControlSet\Services\{s}"), "/v", "Start", "/t", "REG_DWORD", "/d", &v.to_string(), "/f"]));
        if on && off == 4 {
            let _ = run("sc.exe", &["stop", s]); // the per-user cbdhsvc_xxxx instance stops at the next sign-in
        }
    }
    // launchers up to v0.2.1 disabled DiagTrack here: put it back, unless debloat is the one keeping it off
    if !on && !db_dir().join("debloat.exe").is_file() {
        soft("DiagTrack", run("reg", &["add", r"HKLM\SYSTEM\CurrentControlSet\Services\DiagTrack", "/v", "Start", "/t", "REG_DWORD", "/d", "2", "/f"]));
    }
    let t = if on { 0x4000000 } else { SPLIT_DEFAULT };
    soft("SvcHostSplitThresholdInKB", run("reg", &["add", SPLIT_KEY, "/v", "SvcHostSplitThresholdInKB", "/t", "REG_DWORD", "/d", &t.to_string(), "/f"]));
}

/// In our Program Files folder (admin-only): a .reg a user could plant would be imported by the elevated uninstall.
pub(crate) fn rx_folders_bak(i: usize) -> PathBuf {
    rx_dir().join(format!("folders-{i}.reg"))
}

pub(crate) fn rx_lnk() -> PathBuf {
    program_data().join(r"plugin-launcher\rs-overlay.lnk")
}

pub(crate) fn rx_install() -> Result<(), String> {
    if !bar_dir().join("topbar.exe").is_file() {
        return Err("install topbar first: without it there will be no bar after Explorer is disabled".into());
    }
    with_payload(&["rust-explorer.exe", "rs-shell.exe"], |d| {
        for n in ["rs-shell.exe", "rust-explorer.exe"] {
            let _ = run("taskkill", &["/f", "/im", n]); // an update: a running exe cannot be overwritten
        }
        fs::create_dir_all(rx_dir()).map_err(|e| e.to_string())?;
        let (fm, sh) = (rx_dir().join("rust-explorer.exe"), rx_dir().join("rs-shell.exe"));
        put(&d.join("rust-explorer.exe"), &fm)?;
        put(&d.join("rs-shell.exe"), &sh)?;
        // what Windows had, saved once (an update must not overwrite it with our own 0)
        if reg_dword(RX_STATE, "AutoRestartShell").is_none() {
            let v = reg_dword(WINLOGON_KEY, "AutoRestartShell").unwrap_or(1);
            run("reg", &["add", RX_STATE, "/v", "AutoRestartShell", "/t", "REG_DWORD", "/d", &v.to_string(), "/f"])?;
        }
        // folders and drives opened by other programs, the desktop, Win+R: rust-explorer. DelegateExecute is emptied on purpose:
        // while it is set, Windows ignores the command and uses Explorer's built-in handler.
        // Another file manager's verbs (Directory Opus, Total Commander) are exported first, once, and imported back on uninstall.
        for (i, k) in RX_FOLDERS.iter().enumerate() {
            let (bak, ours) = (rx_folders_bak(i), out("reg", &["query", &format!(r"{k}\command"), "/ve"]).is_ok_and(|c| c.to_lowercase().contains("rust-explorer")));
            if !bak.is_file() && !ours && run("reg", &["query", k]).is_ok() {
                run("reg", &["export", k, &bak.display().to_string(), "/y"])?;
            }
        }
        for k in RX_FOLDERS {
            let c = format!(r"{k}\command");
            run("reg", &["add", &c, "/ve", "/t", "REG_SZ", "/d", &format!("\"{}\" \"%1\"", fm.display()), "/f"])?;
            run("reg", &["add", &c, "/v", "DelegateExecute", "/t", "REG_SZ", "/d", "", "/f"])?;
        }
        // A logon task starts the overlay right at sign-in; Explorer runs the Run key only after its own startup delay (~20 s of
        // plain Windows on screen). Limited run level: the shell side must never run elevated. Fallback: the Run key.
        let task = format!("{} Register-ScheduledTask -TaskName rs-overlay -Force -Action (New-ScheduledTaskAction -Execute '{}' -Argument '--overlay') -Trigger (New-ScheduledTaskTrigger -AtLogOn -User $u) -Principal (New-ScheduledTaskPrincipal -UserId $u -LogonType Interactive -RunLevel Limited) -Settings (New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit ([TimeSpan]::Zero)) | Out-Null", task_user(), sh.display());
        if run("powershell", &["-NoProfile", "-Command", &task]).is_ok() {
            let _ = run("reg", &["delete", RUN_KEY, "/v", "rs-overlay", "/f"]);
        } else {
            WARN.lock().unwrap().push("rs-overlay task was not created: it starts via Run, with a Windows startup delay".into());
            run("reg", &["add", RUN_KEY, "/v", "rs-overlay", "/t", "REG_SZ", "/d", &format!("\"{}\" --overlay", sh.display()), "/f"])?;
        }
        run("reg", &["add", RX_SWITCH, "/v", "ReplaceExplorer", "/t", "REG_DWORD", "/d", "1", "/f"])?;
        run("reg", &["add", WINLOGON_KEY, "/v", "AutoRestartShell", "/t", "REG_DWORD", "/d", "0", "/f"])?;
        rx_trim(true);
        // the bar's Start panel and the search list Start Menu shortcuts only: without one rust-explorer has no entry (and no icon) there
        soft("Start menu shortcut 'Rust Explorer' was not created", shortcut(&start_lnk("Rust Explorer"), &fm, ""));
        // Start it now, with the user's normal token: explorer.exe opens a shortcut unelevated, and the shortcut carries --overlay.
        let lnk = rx_lnk();
        let _ = fs::create_dir_all(lnk.parent().unwrap());
        soft("overlay is not running now: it starts at the next logon", shortcut(&lnk, &sh, "--overlay"));
        let _ = Command::new("explorer.exe").arg(&lnk).spawn();
        Ok(())
    })
}

pub(crate) fn rx_uninstall() -> Result<(), String> {
    // Windows may restart its shell again; our hooks off; then the shell back, unelevated, only if none is running
    let back = reg_dword(RX_STATE, "AutoRestartShell").unwrap_or(1);
    let r = run("reg", &["add", WINLOGON_KEY, "/v", "AutoRestartShell", "/t", "REG_DWORD", "/d", &back.to_string(), "/f"]);
    let _ = run("reg", &["delete", RUN_KEY, "/v", "rs-overlay", "/f"]);
    let _ = run("schtasks", &["/delete", "/tn", "rs-overlay", "/f"]);
    let _ = run("reg", &["delete", RX_SWITCH, "/f"]);
    rx_trim(false);
    for (i, k) in RX_FOLDERS.iter().enumerate() {
        let _ = run("reg", &["delete", k, "/f"]); // only our HKCU override; the system's verbs under HKLM stay
        let bak = rx_folders_bak(i);
        if bak.is_file() {
            soft(&format!("folder verbs {k} were not restored"), run("reg", &["import", &bak.display().to_string()]));
        }
    }
    for n in ["rs-shell.exe", "rust-explorer.exe"] {
        let _ = run("taskkill", &["/f", "/im", n]);
    }
    // the shell back with the user's normal token: this uninstaller is elevated, and runas /trustlevel from here gave
    // Explorer an elevated-type token, under which Store apps (Calculator) open suspended and hidden (winrust_common::shell)
    let ps = format!(r#"if (-not (Get-CimInstance Win32_Process -Filter "Name='explorer.exe'" | Where-Object {{ $_.CommandLine -match '(?i)explorer\.exe"?(\s+/NoUACCheck)?\s*$' }})) {{ {} }}"#, i18n::shell::START_EXPLORER_PS);
    let _ = run("powershell", &["-NoProfile", "-Command", &ps]);
    let _ = fs::remove_file(rx_lnk());
    let _ = fs::remove_file(start_lnk("Rust Explorer"));
    let _ = fs::remove_dir_all(rx_dir());
    if r.is_ok() {
        let _ = run("reg", &["delete", RX_STATE, "/f"]);
    }
    r
}
