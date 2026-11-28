//! Install logic, shared by the window and the command line so both do exactly the same thing:
//! process/registry helpers, the embedded payload, and one file per plugin with its install/uninstall pair.
use crate::*;
mod explorer;
mod search;
mod session;
mod taskmgr;
mod tools;
mod topbar;
pub(crate) use {explorer::*, search::*, session::*, taskmgr::*, tools::*, topbar::*};

pub(crate) fn out(prog: &str, args: &[&str]) -> Result<String, String> {
    // reg.exe: HKCU means the signed-in user's, not the elevated account's (see session.rs)
    let owned: Vec<String> = if prog == "reg" { i18n::reg::user_args(args) } else { args.iter().map(|a| a.to_string()).collect() };
    let o = Command::new(prog).args(&owned).creation_flags(0x0800_0000).output().map_err(|e| format!("{prog}: {e}"))?;
    if o.status.success() {
        Ok(String::from_utf8_lossy(&o.stdout).into_owned())
    } else {
        Err(format!("{prog} {}: {}{}", owned.join(" "), String::from_utf8_lossy(&o.stdout).trim(), String::from_utf8_lossy(&o.stderr).trim()))
    }
}

pub(crate) fn run(prog: &str, args: &[&str]) -> Result<(), String> {
    out(prog, args).map(|_| ())
}

// Non-fatal failures of one install: shown as a count in the status line, spelled out in install.log.
pub(crate) static WARN: Mutex<Vec<String>> = Mutex::new(Vec::new());

pub(crate) fn soft(what: &str, r: Result<(), String>) {
    if let Err(e) = r {
        WARN.lock().unwrap().push(format!("{what}: {e}"));
    }
}

pub(crate) fn program_data() -> PathBuf {
    PathBuf::from(env::var("ProgramData").unwrap_or_else(|_| r"C:\ProgramData".into()))
}

/// Start Menu shortcut: search and the bar's Start panel list Start Menu shortcuts only.
pub(crate) fn start_lnk(name: &str) -> PathBuf {
    program_data().join(format!(r"Microsoft\Windows\Start Menu\Programs\{name}.lnk"))
}

pub(crate) fn shortcut(lnk: &Path, target: &Path, args: &str) -> Result<(), String> {
    run("powershell", &["-NoProfile", "-Command", &format!("$s=(New-Object -ComObject WScript.Shell).CreateShortcut('{}'); $s.TargetPath='{}'; $s.Arguments='{args}'; $s.Save()", lnk.display(), target.display())])
}

pub(crate) fn log(s: &str) {
    let d = program_data().join("plugin-launcher");
    let _ = fs::create_dir_all(&d);
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(d.join("install.log")) {
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let _ = writeln!(f, "[{t}] {s}"); // shortcut: unix seconds, no date formatting; upgrade if the log gets read by humans a lot
    }
}

/// Copies over an exe that was just killed: taskkill returns before Windows (or an antivirus scan) releases the file, so retry ~6 s.
/// A process can also stay stuck while it closes (dead, one thread left waiting in a driver) and keep its exe locked for
/// as long as Windows runs. Then the old exe is moved aside (`name.exe.old-<time>`: a running exe may be renamed, not
/// overwritten) and the new one copied in; the leftovers are deleted on a later install, once nothing holds them.
pub(crate) fn put(src: &Path, dst: &Path) -> Result<(), String> {
    let in_use = |r: &std::io::Result<()>| matches!(r, Err(e) if e.raw_os_error() == Some(32) || e.raw_os_error() == Some(5));
    remove_moved_aside(dst);
    let mut r = fs::copy(src, dst).map(|_| ());
    for _ in 0..30 {
        if !in_use(&r) {
            break;
        }
        thread::sleep(std::time::Duration::from_millis(200));
        r = fs::copy(src, dst).map(|_| ());
    }
    if in_use(&r) && dst.is_file() {
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let aside = dst.with_file_name(format!("{}.old-{t}", dst.file_name().unwrap_or_default().to_string_lossy()));
        if fs::rename(dst, &aside).is_ok() {
            log(&format!("{}: in use, the old one moved aside to {}", dst.display(), aside.display()));
            r = fs::copy(src, dst).map(|_| ());
            if r.is_err() {
                let _ = fs::rename(&aside, dst); // the new one did not go in: the old one back where it was
            }
        }
    }
    r.map_err(|e| format!("{}: {e}", dst.display()))
}

/// Deletes the copies of `dst` that `put` moved aside before (those still locked stay for the next time).
fn remove_moved_aside(dst: &Path) {
    let (Some(dir), Some(name)) = (dst.parent(), dst.file_name()) else { return };
    let prefix = format!("{}.old-", name.to_string_lossy()).to_lowercase();
    for e in fs::read_dir(dir).into_iter().flatten().flatten() {
        if e.file_name().to_string_lossy().to_lowercase().starts_with(&prefix) {
            let _ = fs::remove_file(e.path());
        }
    }
}

pub(crate) fn reg_dword(key: &str, name: &str) -> Option<u32> {
    i18n::reg::dword(&out("reg", &["query", key, "/v", name]).ok()?)
}

/// A REG_SZ (or any string-like) value as text.
pub(crate) fn reg_str(key: &str, name: &str) -> Option<String> {
    i18n::reg::value(&out("reg", &["query", key, "/v", name]).ok()?).map(|(_, d)| d.to_string())
}

include!(concat!(env!("OUT_DIR"), "/payload.rs")); // PAYLOAD: the exes listed in payload.txt, embedded by build.rs

/// Update without rebuilding the launcher: an exe in `payload\` next to this one (or two levels up, the cargo
/// target\release layout) wins over the embedded copy. Trust level = the launcher exe's own folder. Two levels up only from
/// target\release: from C:\Program Files\WinRust it would be C:\payload, which any user may create.
pub(crate) fn disk_payload() -> Option<PathBuf> {
    let exe = env::current_exe().ok()?;
    let d = exe.parent()?;
    [Some(d.join("payload")), d.ends_with(r"target\release").then(|| d.join(r"..\..\payload"))].into_iter().flatten().find(|p| p.is_dir())
}

/// Unpacks the `need`ed bundled plugins, runs `f` on them, deletes them. The folder is under Program Files (admin-only writable),
/// not %TEMP%: a user-level process must not be able to swap an exe that is about to be installed as a SYSTEM task.
pub(crate) fn with_payload<T>(need: &[&str], f: impl FnOnce(&Path) -> Result<T, String>) -> Result<T, String> {
    let d = program_files().join(format!("plugin-launcher-tmp-{}", std::process::id()));
    fs::create_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    let disk = disk_payload();
    let unpack = |n: &&str| -> Result<(), String> {
        let bytes = PAYLOAD.iter().find(|(m, _)| m == n).ok_or(format!("{n} is missing from the build"))?.1;
        match disk.as_ref().map(|p| p.join(n)).filter(|p| p.is_file()) {
            Some(src) => {
                log(&format!("{n}: from {}", src.display()));
                fs::copy(src, d.join(n)).map(|_| ())
            }
            None => fs::write(d.join(n), bytes),
        }
        .map_err(|e| format!("unpacking {n}: {e}"))
    };
    let r = need.iter().try_for_each(unpack).and_then(|_| f(&d));
    let _ = fs::remove_dir_all(&d);
    r
}

pub(crate) fn program_files() -> PathBuf {
    PathBuf::from(env::var("ProgramFiles").unwrap_or(r"C:\Program Files".into()))
}

// shortcut: Windows makes at most one restore point per 24 h and it needs System Protection on; failures are ignored.
pub(crate) fn checkpoint() -> Result<(), String> {
    run("powershell", &["-NoProfile", "-Command", "Checkpoint-Computer -Description 'Plugin Launcher' -RestorePointType MODIFY_SETTINGS"])
}

// Shared by the window and the command line, so both do exactly the same thing.
pub(crate) fn install_plugin(i: usize) -> Result<String, String> {
    let p = &PLUGINS[i];
    let (inst, uninst) = p.actions.ok_or("installation is not available")?;
    init().inspect_err(|e| log(&format!("{}: ERROR {e}", p.name)))?; // before anything is written: no user, no install
    let was = (p.installed)(); // already installed = an update: a failure must not remove the working copy
    WARN.lock().unwrap().clear();
    soft("restore point was not created", checkpoint());
    if let Err(e) = inst() {
        log(&format!("{}: ERROR {e}", p.name));
        if was {
            return Err(e);
        }
        let back = uninst().err().map_or(String::new(), |u| format!("; rollback: {u}"));
        return Err(format!("{e} (installation cancelled, changes reverted{back})"));
    }
    let w = WARN.lock().unwrap().clone();
    log(&format!("{}: {} {}", p.name, if was { "updated" } else { "installed" }, w.join(" | ")));
    let note = if w.is_empty() { String::new() } else { format!(" Warnings: {} (see %ProgramData%\\plugin-launcher\\install.log).", w.len()) };
    Ok(format!("{} {}.{note}", p.name, if was { "updated" } else { "installed" }))
}

pub(crate) fn uninstall_plugin(i: usize) -> Result<String, String> {
    let p = &PLUGINS[i];
    let (_, uninst) = p.actions.ok_or("uninstall is not available")?;
    WARN.lock().unwrap().clear();
    init_soft();
    uninst().inspect_err(|e| log(&format!("{}: uninstall ERROR {e}", p.name)))?;
    let w = WARN.lock().unwrap().clone();
    log(&format!("{}: {} {}", p.name, "removed", w.join(" | ")));
    let note = if w.is_empty() { String::new() } else { format!(" Warnings: {} (see %ProgramData%\\plugin-launcher\\install.log).", w.len()) };
    Ok(format!("{} removed, system restored.{note}", p.name))
}

pub(crate) fn rollback() -> Result<String, String> {
    init_soft();
    // reverse install order: rust-explorer (which switched Explorer off) goes before topbar, else the session is left with no panel and no shell
    let errs: Vec<String> = PLUGINS.iter().rev().filter(|p| (p.installed)()).filter_map(|p| p.actions.and_then(|a| a.1().err().map(|e| format!("{}: {e}", p.name)))).collect();
    if errs.is_empty() { Ok("All plugins removed, system restored.".into()) } else { Err(errs.join("; ")) }
}
