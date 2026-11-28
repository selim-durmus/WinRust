//! Who has a file or folder open: the programs and services that stop a delete or move ("File in use"), and ending them.
//! Two sources. Restart Manager is what Explorer asks: files only, and it sees elevated programs and services too. A walk
//! over every open handle in the system adds what it misses: a program whose current folder is the one being deleted, or
//! one holding a folder open (only processes this user may open, so not elevated ones while rust-explorer runs without
//! admin). Ending an elevated program or stopping a service starts this exe as admin (`--rx-end`, `--rx-stop`): one UAC.
#![allow(unsafe_op_in_unsafe_fn)]
use std::{collections::{BTreeMap, HashMap}, ffi::c_void, os::windows::ffi::OsStrExt, path::{Path, PathBuf}, ptr, time::{Duration, Instant}};
use windows_sys::Win32::{Foundation::*, Storage::FileSystem::*, System::{RestartManager::*, Services::*, Threading::*}, UI::Shell::*};

/// A program holding something open.
#[derive(Clone, Debug, Default)]
pub struct Holder {
    pub pid: u32,
    pub app: String,                    // Restart Manager's name for it, else the exe's description ("Windows Command Processor")
    pub exe: Option<PathBuf>,
    pub services: Vec<(String, String)>, // (name, display name) of the services running in it
    pub critical: bool,                 // a part of Windows that must not be ended
}

impl Holder {
    /// What the program is called: its service, Restart Manager's name, else the exe.
    pub fn title(&self) -> String {
        if !self.services.is_empty() { return self.services.iter().map(|s| s.1.as_str()).collect::<Vec<_>>().join(", "); }
        if !self.app.is_empty() { return self.app.clone(); }
        self.exe_name()
    }
    pub fn exe_name(&self) -> String {
        self.exe.as_ref().and_then(|e| e.file_name()).map_or_else(|| format!("Process {}", self.pid), |n| n.to_string_lossy().into_owned())
    }
    /// What can be done about it: stop its service, end it, or nothing (a part of Windows, or several services share it).
    pub fn action(&self) -> Option<Action> {
        if self.critical { return None; }
        match self.services.len() { 0 => Some(Action::End), 1 => Some(Action::Stop), _ => None }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Action { End, Stop }

/// Processes that keep Windows running: never offered for ending (ending some of them stops the PC).
const CRITICAL: [&str; 10] = ["system", "smss.exe", "csrss.exe", "wininit.exe", "winlogon.exe", "services.exe", "lsass.exe", "lsaiso.exe", "dwm.exe", "fontdrvhost.exe"];

/// Files of a folder looked at by Restart Manager, at most.
const MAX_FILES: usize = 4000;

fn wide(p: &Path) -> Vec<u16> { p.as_os_str().encode_wide().chain([0]).collect() }

fn utf16(b: &[u16]) -> String { String::from_utf16_lossy(&b[..b.iter().position(|&c| c == 0).unwrap_or(b.len())]) }

/// Everything (but this process) that has one of `paths` open, or something inside one of them.
pub fn who(paths: &[PathBuf]) -> Vec<Holder> {
    let me = std::process::id();
    let mut found: BTreeMap<u32, Holder> = BTreeMap::new();
    let (mut files, mut dirs) = (vec![], false);
    for p in paths {
        if p.is_dir() { dirs = true; walk(p, &mut files); } else if p.exists() { files.push(p.clone()); }
    }
    for i in unsafe { restart_manager(&files) } {
        let pid = i.Process.dwProcessId;
        let h = found.entry(pid).or_insert_with(|| Holder { pid, ..Default::default() });
        h.app = utf16(&i.strAppName);
        h.critical |= i.ApplicationType == RmCritical;
    }
    // folders (a program's current folder, a folder handle) are only seen by the handle walk
    if dirs || found.is_empty() {
        for pid in unsafe { handle_walk(paths, &found) } { found.entry(pid).or_insert_with(|| Holder { pid, ..Default::default() }); }
    }
    found.remove(&me);
    let services = unsafe { services_by_pid() };
    found.into_values().map(|mut h| {
        h.exe = unsafe { exe_of(h.pid) };
        if h.app.is_empty() { h.app = h.exe.as_deref().and_then(|e| unsafe { description(e) }).unwrap_or_default(); }
        h.services = services.get(&h.pid).cloned().unwrap_or_default();
        let name = h.exe_name().to_lowercase();
        h.critical |= h.pid <= 4 || CRITICAL.contains(&name.as_str());
        if h.app.eq_ignore_ascii_case(&h.exe_name()) { h.app.clear(); }
        h
    }).collect()
}

/// The files below `dir` (not following links), up to MAX_FILES in all.
fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            if out.len() >= MAX_FILES { return; }
            let Ok(t) = e.file_type() else { continue };
            if t.is_symlink() { continue; }
            if t.is_dir() { todo.push(e.path()); } else { out.push(e.path()); }
        }
    }
}

/// Restart Manager's list for `files`: who has them open (programs, services, elevated or not).
unsafe fn restart_manager(files: &[PathBuf]) -> Vec<RM_PROCESS_INFO> {
    if files.is_empty() { return vec![]; }
    let (mut s, mut key) = (0u32, [0u16; CCH_RM_SESSION_KEY as usize + 1]);
    if RmStartSession(&mut s, 0, key.as_mut_ptr()) != 0 { return vec![]; }
    let names: Vec<Vec<u16>> = files.iter().map(|f| wide(f)).collect();
    let ptrs: Vec<*const u16> = names.iter().map(|n| n.as_ptr()).collect();
    for c in ptrs.chunks(512) { RmRegisterResources(s, c.len() as u32, c.as_ptr(), 0, ptr::null(), 0, ptr::null()); }
    let mut v: Vec<RM_PROCESS_INFO> = vec![];
    let mut n = 0u32;
    for _ in 0..4 {
        let (mut need, mut reasons) = (0u32, 0u32);
        n = v.len() as u32;
        let r = RmGetList(s, &mut need, &mut n, if v.is_empty() { ptr::null_mut() } else { v.as_mut_ptr() }, &mut reasons);
        if r == ERROR_MORE_DATA { v = vec![std::mem::zeroed(); need as usize + 4]; continue; }
        if r != 0 { n = 0; }
        break;
    }
    RmEndSession(s);
    v.truncate(n as usize);
    v
}

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtQuerySystemInformation(class: u32, info: *mut c_void, len: u32, ret: *mut u32) -> i32;
}

/// One entry of SystemExtendedHandleInformation.
#[repr(C)]
struct HandleEntry { object: usize, pid: usize, handle: usize, access: u32, backtrace: u16, kind: u16, attrs: u32, reserved: u32 }

/// The real path behind an open file handle, lower case, without the `\\?\` prefix.
unsafe fn final_path(h: HANDLE) -> Option<String> {
    let mut b = vec![0u16; 1024];
    let n = GetFinalPathNameByHandleW(h, b.as_mut_ptr(), b.len() as u32, 0) as usize;
    if n == 0 || n >= b.len() { return None; }
    let s = String::from_utf16_lossy(&b[..n]).to_lowercase();
    Some(if let Some(r) = s.strip_prefix(r"\\?\unc\") { format!(r"\\{r}") } else { s.strip_prefix(r"\\?\").map_or(s.clone(), String::from) })
}

/// `p` as the handle walk sees paths (its real path, links and short names resolved), lower case.
unsafe fn normal(p: &Path) -> String {
    let h = CreateFileW(wide(p).as_ptr(), 0, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, ptr::null(), OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, ptr::null_mut());
    let s = if h != INVALID_HANDLE_VALUE { let s = final_path(h); CloseHandle(h); s } else { None };
    s.unwrap_or_else(|| p.to_string_lossy().to_lowercase()).trim_end_matches('\\').to_string()
}

/// Processes with a handle open on one of `paths` or anything below them (those in `skip` are known already). Looks
/// at each process this user may open; gives up after a few seconds on a very busy system.
unsafe fn handle_walk(paths: &[PathBuf], skip: &BTreeMap<u32, Holder>) -> Vec<u32> {
    let targets: Vec<String> = paths.iter().map(|p| normal(p)).collect();
    let hits = |p: &str| targets.iter().any(|t| p == t || (p.starts_with(t.as_str()) && p.as_bytes().get(t.len()) == Some(&b'\\')));
    // which object type a file handle is: the one of a file this process opens itself (open while the table is read)
    let me = std::process::id() as usize;
    let exe = std::env::current_exe().unwrap_or_default();
    let mine = CreateFileW(wide(&exe).as_ptr(), 0, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, ptr::null(), OPEN_EXISTING, 0, ptr::null_mut());
    if mine == INVALID_HANDLE_VALUE { return vec![]; }
    // the system's handle table (it grows while asked: retry with more room)
    let mut buf: Vec<usize> = vec![0; 1 << 19];
    let ok = loop {
        let mut ret = 0u32;
        let st = NtQuerySystemInformation(64, buf.as_mut_ptr() as *mut c_void, (buf.len() * size_of::<usize>()) as u32, &mut ret);
        if st == 0xC000_0004u32 as i32 && buf.len() < (1 << 26) { buf = vec![0; buf.len() * 2]; continue; }
        break st == 0;
    };
    CloseHandle(mine);
    if !ok { return vec![]; }
    let count = buf[0];
    let entries = std::slice::from_raw_parts(buf.as_ptr().add(2) as *const HandleEntry, count.min((buf.len() - 2) * size_of::<usize>() / size_of::<HandleEntry>()));
    let file_kind = entries.iter().find(|e| e.pid == me && e.handle == mine as usize).map(|e| e.kind);
    let Some(file_kind) = file_kind else { return vec![] };
    let (start, mut procs, mut out) = (Instant::now(), HashMap::<usize, HANDLE>::new(), vec![]);
    for e in entries {
        if e.kind != file_kind || e.pid == me || e.pid <= 4 || skip.contains_key(&(e.pid as u32)) || out.contains(&(e.pid as u32)) { continue; }
        // access masks of named pipes that can hang a query
        if matches!(e.access, 0x0012_019F | 0x001A_019F | 0x0012_0189 | 0x0010_0000) { continue; }
        if start.elapsed() > Duration::from_secs(4) { break; }
        let ph = *procs.entry(e.pid).or_insert_with(|| OpenProcess(PROCESS_DUP_HANDLE, 0, e.pid as u32));
        if ph.is_null() { continue; }
        let mut dup: HANDLE = ptr::null_mut();
        if DuplicateHandle(ph, e.handle as HANDLE, GetCurrentProcess(), &mut dup, 0, 0, DUPLICATE_SAME_ACCESS) == 0 { continue; }
        if GetFileType(dup) == FILE_TYPE_DISK && final_path(dup).is_some_and(|p| hits(&p)) { out.push(e.pid as u32); }
        CloseHandle(dup);
    }
    for (_, h) in procs { if !h.is_null() { CloseHandle(h); } }
    out
}

/// Running services by the process they run in: (name, display name).
unsafe fn services_by_pid() -> HashMap<u32, Vec<(String, String)>> {
    let mut out: HashMap<u32, Vec<(String, String)>> = HashMap::new();
    let scm = OpenSCManagerW(ptr::null(), ptr::null(), SC_MANAGER_ENUMERATE_SERVICE);
    if scm.is_null() { return out; }
    let mut buf: Vec<u64> = vec![0; 32 * 1024];
    for _ in 0..3 {
        let (mut need, mut n, mut resume) = (0u32, 0u32, 0u32);
        let ok = EnumServicesStatusExW(scm, SC_ENUM_PROCESS_INFO, SERVICE_WIN32, SERVICE_ACTIVE, buf.as_mut_ptr() as *mut u8, (buf.len() * 8) as u32, &mut need, &mut n, &mut resume, ptr::null());
        if ok == 0 && GetLastError() == ERROR_MORE_DATA { buf = vec![0; buf.len() + need as usize / 8 + 1024]; continue; }
        for s in std::slice::from_raw_parts(buf.as_ptr() as *const ENUM_SERVICE_STATUS_PROCESSW, n as usize) {
            let pid = s.ServiceStatusProcess.dwProcessId;
            if pid == 0 { continue; }
            let text = |p: *mut u16| { let mut k = 0; while *p.add(k) != 0 { k += 1; } String::from_utf16_lossy(std::slice::from_raw_parts(p, k)) };
            out.entry(pid).or_default().push((text(s.lpServiceName), text(s.lpDisplayName)));
        }
        break;
    }
    CloseServiceHandle(scm);
    out
}

/// The program file a process runs.
unsafe fn exe_of(pid: u32) -> Option<PathBuf> {
    let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
    if h.is_null() { return None; }
    let mut b = [0u16; 1024];
    let mut n = b.len() as u32;
    let ok = QueryFullProcessImageNameW(h, 0, b.as_mut_ptr(), &mut n) != 0;
    CloseHandle(h);
    ok.then(|| PathBuf::from(String::from_utf16_lossy(&b[..n as usize])))
}

/// The "File description" of a program file (what Task Manager shows as its name).
unsafe fn description(exe: &Path) -> Option<String> {
    let name = wide(exe);
    let n = GetFileVersionInfoSizeW(name.as_ptr(), ptr::null_mut());
    if n == 0 { return None; }
    let mut data = vec![0u8; n as usize];
    if GetFileVersionInfoW(name.as_ptr(), 0, n, data.as_mut_ptr() as *mut c_void) == 0 { return None; }
    let query = |key: &str| -> Option<(*const u16, usize)> {
        let k: Vec<u16> = key.encode_utf16().chain([0]).collect();
        let (mut p, mut len) = (ptr::null_mut::<c_void>(), 0u32);
        (VerQueryValueW(data.as_ptr() as *const c_void, k.as_ptr(), &mut p, &mut len) != 0 && !p.is_null() && len > 0).then_some((p as *const u16, len as usize))
    };
    // the first language the file has, then US English / Unicode
    let lang = query(r"\VarFileInfo\Translation").map(|(p, _)| { let w = p as *const u16; format!("{:04x}{:04x}", *w, *w.add(1)) });
    for l in lang.into_iter().chain(["040904b0".to_string(), "040904e4".to_string()]) {
        if let Some((p, len)) = query(&format!(r"\StringFileInfo\{l}\FileDescription")) {
            let s = utf16(std::slice::from_raw_parts(p, len)).trim().to_string();
            if !s.is_empty() { return Some(s); }
        }
    }
    None
}

// ---------- ending what holds it ----------

/// Ends process `pid`, or stops service `name`: here when this process may, else through this exe started as admin.
/// Err holds what to tell the user.
pub fn act(a: Action, h: &Holder) -> Result<(), String> {
    let (here, arg) = match a {
        Action::End => (unsafe { end_here(h.pid) }, h.pid.to_string()),
        Action::Stop => { let n = h.services.first().map_or(String::new(), |s| s.0.clone()); (unsafe { stop_here(&n) }, n) }
    };
    match here {
        0 => Ok(()),
        ERROR_ACCESS_DENIED => unsafe { as_admin(if a == Action::End { "--rx-end" } else { "--rx-stop" }, &arg) },
        e => Err(message(e)),
    }
}

/// Ends `pid` and waits for it to be gone. 0 or the Windows error.
unsafe fn end_here(pid: u32) -> u32 {
    let h = OpenProcess(PROCESS_TERMINATE | PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
    if h.is_null() { return GetLastError(); }
    let critical = exe_of(pid).and_then(|e| e.file_name().map(|n| n.to_string_lossy().to_lowercase())).is_some_and(|n| CRITICAL.contains(&n.as_str()));
    let r = if critical { ERROR_ACCESS_DENIED } else if TerminateProcess(h, 1) == 0 { GetLastError() } else { WaitForSingleObject(h, 5000); 0 };
    CloseHandle(h);
    r
}

/// Stops service `name` and waits (up to 20 s) until it has. 0 or the Windows error.
unsafe fn stop_here(name: &str) -> u32 {
    let scm = OpenSCManagerW(ptr::null(), ptr::null(), SC_MANAGER_CONNECT);
    if scm.is_null() { return GetLastError(); }
    let n: Vec<u16> = name.encode_utf16().chain([0]).collect();
    let s = OpenServiceW(scm, n.as_ptr(), SERVICE_STOP | SERVICE_QUERY_STATUS);
    let r = if s.is_null() { GetLastError() } else {
        let mut st: SERVICE_STATUS = std::mem::zeroed();
        let mut r = if ControlService(s, SERVICE_CONTROL_STOP, &mut st) == 0 { GetLastError() } else { 0 };
        if r == ERROR_SERVICE_NOT_ACTIVE { r = 0; }
        let t = Instant::now();
        while r == 0 && t.elapsed() < Duration::from_secs(20) {
            if QueryServiceStatus(s, &mut st) == 0 || st.dwCurrentState == SERVICE_STOPPED { break; }
            std::thread::sleep(Duration::from_millis(200));
        }
        if r == 0 && st.dwCurrentState != SERVICE_STOPPED { r = ERROR_SERVICE_REQUEST_TIMEOUT; }
        CloseServiceHandle(s);
        r
    };
    CloseServiceHandle(scm);
    r
}

/// Runs this exe as admin with `flag arg` (one UAC prompt) and waits for it.
unsafe fn as_admin(flag: &str, arg: &str) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let (file, verb, params) = (wide(&exe), wide(Path::new("runas")), wide(Path::new(&format!("{flag} \"{arg}\""))));
    let mut sei: SHELLEXECUTEINFOW = std::mem::zeroed();
    sei.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
    sei.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC;
    sei.lpVerb = verb.as_ptr();
    sei.lpFile = file.as_ptr();
    sei.lpParameters = params.as_ptr();
    sei.nShow = 0; // SW_HIDE
    if ShellExecuteExW(&mut sei) == 0 {
        let e = GetLastError();
        return Err(if e == ERROR_CANCELLED { "Cancelled".into() } else { message(e) });
    }
    if sei.hProcess.is_null() { return Err("Could not start as administrator".into()); }
    WaitForSingleObject(sei.hProcess, 30_000);
    let mut code = 1u32;
    GetExitCodeProcess(sei.hProcess, &mut code);
    CloseHandle(sei.hProcess);
    if code == 0 { Ok(()) } else { Err(message(code)) }
}

fn message(e: u32) -> String {
    match e {
        ERROR_ACCESS_DENIED => "Access denied".into(),
        ERROR_INVALID_PARAMETER => "It has already ended".into(),
        ERROR_SERVICE_REQUEST_TIMEOUT => "The service did not stop in time".into(),
        ERROR_DEPENDENT_SERVICES_RUNNING => "Other services depend on it".into(),
        ERROR_INVALID_SERVICE_CONTROL => "The service cannot be stopped".into(),
        e => format!("Failed (error {e})"),
    }
}

/// The admin half: `rust-explorer --rx-end <pid>` / `--rx-stop <service>` does it and exits with 0 or the Windows error.
/// None when started any other way.
pub fn helper() -> Option<i32> {
    let a: Vec<String> = std::env::args().collect();
    let arg = a.get(2)?;
    let r = match a.get(1)?.as_str() {
        "--rx-end" => unsafe { end_here(arg.parse().ok()?) },
        "--rx-stop" => unsafe { stop_here(arg) },
        _ => return None,
    };
    Some(r as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_process_that_has_a_file_open_and_a_folder_as_its_current_folder() {
        let dir = std::env::temp_dir().join(format!("rx-locks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("cwd")).unwrap();
        let file = dir.join("held.txt");
        std::fs::write(&file, "x").unwrap();
        // a file held open with no sharing, by another process
        let mut a = std::process::Command::new("powershell").args(["-NoProfile", "-Command", &format!("$h=[IO.File]::Open('{}','Open','Read','None'); Start-Sleep 20", file.display())]).spawn().unwrap();
        // a folder that another process sits in
        let mut b = std::process::Command::new("cmd").args(["/c", "ping -n 20 127.0.0.1 >nul"]).current_dir(dir.join("cwd")).spawn().unwrap();
        std::thread::sleep(Duration::from_secs(3));
        let f = who(std::slice::from_ref(&file));
        let t = Instant::now();
        let d = who(&[dir.join("cwd")]);
        eprintln!("folder lookup took {} ms", t.elapsed().as_millis());
        let _ = (a.kill(), b.kill(), a.wait(), b.wait());
        let _ = std::fs::remove_dir_all(&dir);
        assert!(f.iter().any(|h| h.pid == a.id()), "file holder not found: {f:?}");
        assert!(d.iter().any(|h| h.pid == b.id()), "folder holder not found: {d:?}");
        let h = f.iter().find(|h| h.pid == a.id()).unwrap();
        assert_eq!(h.exe_name().to_lowercase(), "powershell.exe");
        assert_eq!(h.action(), Some(Action::End));
        // a program Restart Manager does not name is called by its file description, not "cmd.exe" twice
        let c = d.iter().find(|h| h.pid == b.id()).unwrap();
        assert!(!c.app.is_empty() && !c.title().eq_ignore_ascii_case("cmd.exe"), "{c:?}");
    }

    #[test]
    fn parts_of_windows_and_shared_service_hosts_get_no_button() {
        let h = Holder { pid: 600, exe: Some(PathBuf::from(r"C:\Windows\System32\lsass.exe")), critical: true, ..Default::default() };
        assert_eq!(h.action(), None);
        let two = Holder { pid: 900, services: vec![("a".into(), "A".into()), ("b".into(), "B".into())], ..Default::default() };
        assert_eq!(two.action(), None);
        assert_eq!(two.title(), "A, B");
        let one = Holder { pid: 901, services: vec![("VPNUnlimitedService".into(), "VPN Unlimited Service".into())], ..Default::default() };
        assert_eq!(one.action(), Some(Action::Stop));
        assert_eq!(one.title(), "VPN Unlimited Service");
    }
}
