//! rust-search: NTFS search service + Alt+Space launcher; disables Windows Search (reversibly).
use crate::*;

/// What the system looked like before the plugin touched it: HKLM\SOFTWARE\plugin-launcher\<plugin>. Written once
/// (an update must not overwrite the originals with our own values), read and deleted on uninstall.
pub(crate) const STATE: &str = r"HKLM\SOFTWARE\plugin-launcher\rust-search";
pub(crate) const WS_KEY: &str = r"HKLM\SYSTEM\CurrentControlSet\Services\WSearch";
pub(crate) const SB_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Search";

pub(crate) fn save_dword(name: &str, v: Option<u32>) {
    if let (Some(v), None) = (v, reg_dword(STATE, name)) {
        let _ = run("reg", &["add", STATE, "/v", name, "/t", "REG_DWORD", "/d", &v.to_string(), "/f"]);
    }
}

pub(crate) const SH_KEY: &str = r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\SearchHost.exe";

pub(crate) fn prog_dir() -> PathBuf {
    program_files().join("rs-search")
}

pub(crate) fn taskbar_search(mode: u32) -> Result<(), String> {
    run("reg", &["add", r"HKCU\Software\Microsoft\Windows\CurrentVersion\Search", "/v", "SearchboxTaskbarMode", "/t", "REG_DWORD", "/d", &mode.to_string(), "/f"])
}

/// Stripped Windows builds (LTSC ISOs run through a debloater) have no WSearch service: nothing to switch off, not an error.
pub(crate) fn has_wsearch() -> bool {
    run("sc.exe", &["query", "WSearch"]).is_ok()
}

pub(crate) fn install() -> Result<(), String> {
    // originals first: if anything below fails, the rollback restores exactly these
    save_dword("WSearchStart", reg_dword(WS_KEY, "Start"));
    save_dword("WSearchDelayed", Some(reg_dword(WS_KEY, "DelayedAutostart").unwrap_or(0)));
    save_dword("SearchboxMode", reg_dword(SB_KEY, "SearchboxTaskbarMode"));
    with_payload(&["rs.exe", "rs-launcher.exe"], |src| {
        run(src.join("rs.exe").to_str().unwrap(), &["--install"])?; // copies itself to Program Files, registers boot tasks
        let dst = prog_dir().join("rs-launcher.exe"); // must live somewhere stable: --autostart registers its own path
        let _ = run("taskkill", &["/f", "/im", "rs-launcher.exe"]);
        put(&src.join("rs-launcher.exe"), &dst)?;
        run(dst.to_str().unwrap(), &["--autostart"])?;
        // via explorer: it starts the launcher with the desktop user's normal token. Started from here it would
        // inherit admin rights, and every file opened from search would run elevated too.
        let _ = Command::new("explorer.exe").arg(&dst).spawn();
        if has_wsearch() {
            run("sc.exe", &["config", "WSearch", "start=", "disabled"])?;
            let _ = run("sc.exe", &["stop", "WSearch"]);
        } else {
            WARN.lock().unwrap().push("the Windows Search service (WSearch) does not exist on this system: step skipped".into());
        }
        // SearchHost.exe (the Start/Win+S search UI) is not a service, so it gets the same IFEO trick as taskmgr: Windows runs `cmd /c exit` instead.
        run("reg", &["add", SH_KEY, "/v", "Debugger", "/t", "REG_SZ", "/d", "cmd.exe /c exit", "/f"])?;
        let _ = run("taskkill", &["/f", "/im", "SearchHost.exe"]);
        taskbar_search(0) // hide the taskbar search box
    })
}

pub(crate) fn uninstall() -> Result<(), String> {
    let _ = run("taskkill", &["/f", "/im", "rs-launcher.exe"]);
    let _ = run("reg", &["delete", r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run", "/v", "rs-launcher", "/f"]);
    // Windows Search back first: a failure removing our files below must not leave it switched off
    let _ = run("reg", &["delete", SH_KEY, "/v", "Debugger", "/f"]);
    // no saved values (installed by an older launcher): the Windows defaults
    let mode = match (reg_dword(STATE, "WSearchStart"), reg_dword(STATE, "WSearchDelayed")) {
        (Some(2), Some(0)) => "auto",
        (Some(3), _) => "demand",
        (Some(4), _) => "disabled",
        _ => "delayed-auto",
    };
    let svc = if has_wsearch() { run("sc.exe", &["config", "WSearch", "start=", mode]) } else { Ok(()) }; // nothing to restore without the service
    if svc.is_ok() && mode.ends_with("auto") && has_wsearch() {
        let _ = run("sc.exe", &["start", "WSearch"]);
    }
    let r = svc.and(taskbar_search(reg_dword(STATE, "SearchboxMode").unwrap_or(2)));
    if r.is_ok() {
        let _ = run("reg", &["delete", STATE, "/f"]);
    }
    // run the unpacked copy: an exe cannot delete itself, so the installed one would leave its folder behind
    let files = with_payload(&["rs.exe"], |src| run(src.join("rs.exe").to_str().unwrap(), &["--uninstall"]));
    let _ = fs::remove_dir_all(prog_dir());
    r.and(files)
}
