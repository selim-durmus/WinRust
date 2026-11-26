// Cleaner for ordinary users: every item says what it is, how risky it is and how it goes away.
// Safe/Rebuild items regenerate on their own and are picked by default; Ask items (old files, recycle bin, Windows.old)
// are only removed when chosen. Files go to a per-drive quarantine for 30 days (`--restore`, `--purge`).
// shortcuts: Windows.old/SoftwareDistribution may need ownership rights (reported as "partly kept");
// leftovers of uninstalled programs in AppData are not detected; no restore point is created (quarantine instead).
use super::*;
use std::{cmp::Reverse, time::{Duration, SystemTime, UNIX_EPOCH}};
use windows_sys::Win32::System::Console::{ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, GetStdHandle, STD_OUTPUT_HANDLE, SetConsoleMode};

const QUAR_DAYS: u64 = 30;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Risk { Safe, Rebuild, Ask }
#[derive(Clone, Copy, PartialEq, Eq)]
enum How { Wipe, Remove, Quarantine }

struct Item { risk: Risk, how: How, label: String, note: &'static str, paths: Vec<PathBuf>, bytes: u64 }

fn size_of(p: &Path) -> u64 { if p.is_dir() { dir_size(p) } else { p.metadata().map_or(0, |m| m.len()) } }

fn now() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) }

/// True when nothing touched the file (written, read, created) for `days`.
fn quiet_for(m: &fs::Metadata, days: u64) -> bool {
    [m.modified(), m.accessed(), m.created()].into_iter().flatten().max()
        .is_some_and(|t| t.elapsed().is_ok_and(|e| e > Duration::from_secs(days * 86400)))
}

/// A dir is idle when it and its direct children (release/, debug/, .package-lock.json...) are quiet for `days`.
fn idle(p: &Path, days: u64) -> bool {
    p.metadata().is_ok_and(|m| quiet_for(&m, days)) && fs::read_dir(p).into_iter().flatten().flatten().all(|e| e.metadata().is_ok_and(|m| quiet_for(&m, days)))
}

fn subdirs(base: &Path, pred: impl Fn(&str) -> bool) -> Vec<PathBuf> {
    fs::read_dir(base).into_iter().flatten().flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()) && pred(&e.file_name().to_string_lossy())).map(|e| e.path()).collect()
}

/// The known places. Paths that do not exist are dropped later, so rules for software you do not have cost nothing.
fn rules() -> Vec<Item> {
    let var = |k: &str| env::var_os(k).map(PathBuf::from);
    let (local, roam, home, win, pdata) = (var("LOCALAPPDATA"), var("APPDATA"), var("USERPROFILE"), var("SystemRoot"), var("ProgramData"));
    let at = |b: &Option<PathBuf>, rel: &[&str]| -> Vec<PathBuf> { b.iter().flat_map(|b| rel.iter().map(move |r| b.join(r))).collect() };
    let mut v: Vec<Item> = vec![];
    let mut add = |risk, label: &str, note, paths: Vec<PathBuf>| v.push(Item { risk, how: How::Wipe, label: label.into(), note, paths, bytes: 0 });

    add(Risk::Safe, "Temporary files", "Junk left by programs and installers. Programs will recreate what they need.",
        [var("TEMP").into_iter().collect::<Vec<_>>(), at(&win, &["Temp"])].concat());
    add(Risk::Safe, "Error reports and crash dumps", "Program crash data for developers. You do not need it.",
        [at(&pdata, &["Microsoft/Windows/WER/ReportArchive", "Microsoft/Windows/WER/ReportQueue"]), at(&local, &["CrashDumps"]), at(&win, &["Minidump"])].concat());
    add(Risk::Safe, "Windows Update cache", "Updates that are already installed. Windows re-downloads what it needs.",
        at(&win, &["SoftwareDistribution/Download", "ServiceProfiles/NetworkService/AppData/Local/Microsoft/Windows/DeliveryOptimization/Cache"]));
    add(Risk::Safe, "Windows internet cache", "Temporary copies of pages and images for system programs.", at(&local, &["Microsoft/Windows/INetCache"]));

    let web = "Copies of pages and images. Sites open a little slower the first time. Passwords, bookmarks and history are not touched.";
    for (name, rel) in [("Google Chrome", "Google/Chrome/User Data"), ("Microsoft Edge", "Microsoft/Edge/User Data"), ("Yandex Browser", "Yandex/YandexBrowser/User Data"), ("Brave", "BraveSoftware/Brave-Browser/User Data")] {
        let base = local.as_ref().map(|l| l.join(rel)).unwrap_or_default();
        let ps = subdirs(&base, |n| n == "Default" || n.starts_with("Profile ")).into_iter()
            .flat_map(|p| ["Cache/Cache_Data", "Code Cache", "GPUCache", "Service Worker/CacheStorage"].map(|c| p.join(c))).collect();
        add(Risk::Rebuild, name, web, if base.as_os_str().is_empty() { vec![] } else { ps });
    }
    let ff = local.as_ref().map(|l| l.join("Mozilla/Firefox/Profiles")).unwrap_or_default();
    add(Risk::Rebuild, "Firefox", web, if ff.as_os_str().is_empty() { vec![] } else { subdirs(&ff, |_| true).into_iter().map(|p| p.join("cache2")).collect() });
    add(Risk::Rebuild, "Telegram: media cache", "Pictures and videos from chats. They download again when you open a chat.", at(&roam, &["Telegram Desktop/tdata/user_data/media_cache", "Telegram Desktop/tdata/user_data/cache"]));
    add(Risk::Rebuild, "Discord", "Image and UI cache. It will be recreated.", at(&roam, &["discord/Cache", "discord/Code Cache", "discord/GPUCache"]));
    add(Risk::Rebuild, "Microsoft Teams", "UI and attachment cache. It will be recreated.", at(&roam, &["Microsoft/Teams/Cache", "Microsoft/Teams/blob_storage", "Microsoft/Teams/GPUCache", "Microsoft/Teams/tmp"]));
    add(Risk::Rebuild, "Visual Studio Code", "Editor cache. It will be recreated.", at(&roam, &["Code/Cache", "Code/CachedData", "Code/GPUCache", "Code/Code Cache"]));
    add(Risk::Rebuild, "GPU shader cache (NVIDIA, AMD, DirectX)", "Games rebuild it themselves, the first launch of a game may stutter a little.",
        at(&local, &["NVIDIA/DXCache", "NVIDIA/GLCache", "D3DSCache", "AMD/DxCache", "AMD/DxcCache"]));
    add(Risk::Rebuild, "Steam: browser cache", "Cache of Steam's built-in browser. Games are not touched.", at(&local, &["Steam/htmlcache"]));
    add(Risk::Rebuild, "Developer caches (cargo, npm, pip, NuGet)", "Downloaded packages. They download again on the next build.",
        [at(&home, &[".cargo/registry/cache", ".cargo/registry/src"]), at(&local, &["npm-cache", "pip/cache", "NuGet/v3-cache"]), at(&roam, &["npm-cache"])].concat());
    if let Some(w) = win.as_ref().and_then(|w| w.parent()) {
        v.push(Item { risk: Risk::Ask, how: How::Remove, label: "Windows.old (previous Windows version)".into(),
            note: "Only needed to roll back to the old Windows.", paths: vec![w.join("Windows.old")], bytes: 0 });
    }
    v
}

/// Project build dirs: target next to Cargo.toml, node_modules next to package.json (outermost only, never inside apps).
fn projects(recs: &HashMap<u32, Rec>, tot: &HashMap<u32, u64>, drive: char) -> (Vec<PathBuf>, u64) {
    let is_cand = |r: &Rec| r.dir && (r.name.eq_ignore_ascii_case("target") || r.name.eq_ignore_ascii_case("node_modules"));
    let parents: HashSet<u32> = recs.values().filter(|r| is_cand(r)).map(|r| r.parent).collect();
    let marks: HashSet<(u32, String)> = recs.values().filter(|r| !r.dir && parents.contains(&r.parent) && (r.name.eq_ignore_ascii_case("Cargo.toml") || r.name.eq_ignore_ascii_case("package.json"))).map(|r| (r.parent, r.name.to_ascii_lowercase())).collect();
    let ok = |r: &Rec| marks.contains(&(r.parent, if r.name.eq_ignore_ascii_case("target") { "cargo.toml" } else { "package.json" }.into()));
    let cands: HashSet<u32> = recs.iter().filter(|(_, r)| is_cand(r) && ok(r)).map(|(&i, _)| i).collect();
    let (mut out, mut bytes) = (vec![], 0);
    for &id in &cands {
        let (mut p, mut nested) = (recs[&id].parent, false);
        for _ in 0..128 { if cands.contains(&p) { nested = true; break; } if p == ROOT { break; } p = match recs.get(&p) { Some(x) => x.parent, None => break }; }
        if nested { continue; }
        let path = path_of(recs, id, drive).0;
        // installed apps ship their own node_modules (VS Code, Discord, Node, Unity...): deleting those breaks them
        let low = path.to_lowercase();
        if low.contains("\\appdata\\") || low[3..].starts_with("program files") || low[3..].starts_with("windows") { continue; }
        let p = PathBuf::from(path);
        if !idle(&p, 30) { continue; } // still being built or installed into: somebody is working here
        bytes += tot.get(&id).copied().unwrap_or(0);
        out.push(p);
    }
    (out, bytes)
}

/// Old installers and archives lying in Downloads (top level only).
fn old_downloads() -> Option<Item> {
    const EXT: [&str; 7] = ["exe", "msi", "msix", "iso", "zip", "rar", "7z"];
    let dl = env::var_os("USERPROFILE").map(|h| PathBuf::from(h).join("Downloads"))?;
    let paths: Vec<PathBuf> = fs::read_dir(&dl).ok()?.flatten().filter(|e| {
        let p = e.path();
        e.metadata().is_ok_and(|m| m.is_file() && quiet_for(&m, 90)) && p.extension().is_some_and(|x| EXT.contains(&x.to_string_lossy().to_lowercase().as_str()))
    }).map(|e| e.path()).collect();
    Some(Item { risk: Risk::Ask, how: How::Quarantine, label: format!("Old installers and archives in Downloads ({} items, older than 90 days)", paths.len()),
        note: "Most likely the programs are already installed.", paths, bytes: 0 })
}

/// Biggest forgotten videos/archives/installers in the user's own folders (not cloud folders, not AppData).
fn big_old(recs: &HashMap<u32, Rec>, tot: &HashMap<u32, u64>, drive: char, skip: &[PathBuf]) -> Vec<Item> {
    const EXT: [&str; 10] = ["mp4", "mkv", "avi", "mov", "iso", "zip", "rar", "7z", "exe", "msi"];
    const NO: [&str; 6] = ["\\appdata\\", "\\programdata\\", "onedrive", "google drive", "dropbox", "yandex"];
    let mut c: Vec<(u64, u32)> = recs.iter()
        .filter(|(id, r)| !r.dir && tot.get(*id).is_some_and(|&s| s >= 1 << 30) && r.name.rsplit_once('.').is_some_and(|(_, e)| EXT.contains(&e.to_lowercase().as_str())))
        .map(|(&id, _)| (tot[&id], id)).collect();
    c.sort_by_key(|&(s, _)| Reverse(s));
    let mut out = vec![];
    for (s, id) in c {
        let p = PathBuf::from(path_of(recs, id, drive).0);
        let low = p.to_string_lossy().to_lowercase();
        if !low[3..].starts_with("users\\") || NO.iter().any(|k| low.contains(k)) || skip.contains(&p) || !p.metadata().is_ok_and(|m| quiet_for(&m, 180)) { continue; }
        out.push(Item { risk: Risk::Ask, how: How::Quarantine, label: p.to_string_lossy().into(), note: "A large file that has not been opened for a long time.", paths: vec![p], bytes: s });
        if out.len() == 15 { break; }
    }
    out
}

fn vt() -> bool {
    unsafe {
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut m = 0;
        GetConsoleMode(h, &mut m) != 0 && SetConsoleMode(h, m | ENABLE_VIRTUAL_TERMINAL_PROCESSING) != 0
    }
}

/// Enter = defaults, "a" = everything, "n" = nothing, numbers (1-based, any separator) = defaults plus those.
fn select(s: &str, sel: &mut [bool]) {
    match s.trim().to_lowercase().as_str() {
        "n" | "q" => sel.fill(false),
        "a" => sel.fill(true),
        t => for n in t.split(|c: char| !c.is_ascii_digit()).filter_map(|x| x.parse::<usize>().ok()) {
            if let Some(x) = sel.get_mut(n.wrapping_sub(1)) { *x = true; }
        },
    }
}

fn qroot(drive: char) -> PathBuf { PathBuf::from(format!(r"{}:\.rs-du-quarantine", drive.to_ascii_uppercase())) }

/// The list of quarantined items lives next to rs-du.exe (Program Files, admin-only), not in the quarantine folder: under a
/// drive root any user can create and edit files, and a forged row would make this elevated process delete or move any path.
fn mpath(drive: char) -> PathBuf {
    env::current_exe().expect("exe path").with_file_name(format!("quarantine-{}.tsv", drive.to_ascii_uppercase()))
}

/// Closes the quarantine folder to everyone but administrators and SYSTEM (inherited from a drive root, users could edit it).
// shortcut: a folder a user created before rs-du ever ran stays owned by them; the trusted list above keeps that harmless.
fn lock(root: &Path) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| { let _ = std::process::Command::new("icacls").arg(root).args(["/inheritance:r", "/grant:r", "*S-1-5-32-544:(OI)(CI)F", "*S-1-5-18:(OI)(CI)F", "/Q"]).output(); });
}

fn quarantine(p: &Path, stamp: u64, n: usize) -> bool {
    let drive = p.to_string_lossy().chars().next().unwrap_or('C');
    let root = qroot(drive);
    let dir = root.join(stamp.to_string());
    let to = dir.join(format!("{n}_{}", p.file_name().unwrap_or_default().to_string_lossy()));
    let made = fs::create_dir_all(&dir).is_ok();
    if made { lock(&root); }
    let ok = made && fs::rename(p, &to).is_ok();
    if ok && let Ok(mut f) = fs::OpenOptions::new().append(true).create(true).open(mpath(drive)) {
        let _ = writeln!(f, "{stamp}\t{}\t{}", p.display(), to.display());
    }
    ok
}

type Row = (u64, PathBuf, PathBuf); // (when, original path, quarantined path)

/// Rows whose quarantined path is not inside this drive's quarantine folder are dropped: purge deletes that path.
fn parse(text: &str, drive: char) -> Vec<Row> {
    let root = qroot(drive);
    text.lines().filter_map(|l| {
        let mut s = l.splitn(3, '\t');
        Some((s.next()?.parse().ok()?, s.next()?.into(), s.next()?.into()))
    }).filter(|r: &Row| r.2.starts_with(&root) && !r.2.components().any(|c| c == std::path::Component::ParentDir)).collect()
}

fn manifest(drive: char) -> (PathBuf, Vec<Row>) {
    let m = mpath(drive);
    let rows = parse(&fs::read_to_string(&m).unwrap_or_default(), drive);
    (m, rows)
}

fn save(m: &Path, rows: &[Row]) {
    let _ = fs::write(m, rows.iter().map(|(t, a, b)| format!("{t}\t{}\t{}\n", a.display(), b.display())).collect::<String>());
}

fn remove(p: &Path) { let _ = if p.is_dir() { fs::remove_dir_all(p) } else { fs::remove_file(p) }; }

/// Deletes quarantined items older than 30 days (all of them with `all`). This is when the space really comes back.
pub fn purge(drive: char, all: bool) {
    let (m, rows) = manifest(drive);
    let (mut keep, mut freed) = (vec![], 0);
    for r in rows {
        if all || now().saturating_sub(r.0) > QUAR_DAYS * 86400 {
            let before = size_of(&r.2);
            remove(&r.2);
            freed += before.saturating_sub(size_of(&r.2));
            if !r.2.exists() { continue; }
        }
        keep.push(r);
    }
    save(&m, &keep);
    for d in subdirs(&qroot(drive), |_| true) { let _ = fs::remove_dir(d); } // only empty ones go
    if freed > 0 { println!("quarantine emptied: {} freed", human(freed)); }
}

/// Puts quarantined items back where they were.
pub fn restore(drive: char) {
    let (m, rows) = manifest(drive);
    let mut keep = vec![];
    for r in rows {
        if !r.2.exists() { continue; }
        if r.1.exists() { println!("not restored (the place is taken): {}", r.1.display()); keep.push(r); continue; }
        let moved = r.1.parent().is_some_and(|d| fs::create_dir_all(d).is_ok()) && fs::rename(&r.2, &r.1).is_ok();
        println!("{} {}", if moved { "restored" } else { "FAILED" }, r.1.display());
        if !moved { keep.push(r); }
    }
    save(&m, &keep);
}

pub fn clean(recs: &HashMap<u32, Rec>, tot: &HashMap<u32, u64>, drive: char, apply: bool, ask: bool, all: bool, safe_only: bool) {
    let mut items = rules();
    let (proj, pb) = projects(recs, tot, drive);
    items.push(Item { risk: Risk::Rebuild, how: How::Remove, label: "Project build folders (target, node_modules)".into(), note: "They are rebuilt on the next build or npm install.", paths: proj, bytes: pb });
    items.push(Item { risk: Risk::Ask, how: How::Wipe, label: "Recycle Bin".into(), note: "Files you have already deleted.", paths: vec![PathBuf::from(format!("{drive}:\\$Recycle.Bin"))], bytes: 0 }); // per-user subfolders need admin
    let dl = old_downloads();
    let skip = dl.as_ref().map(|d| d.paths.clone()).unwrap_or_default();
    items.extend(dl);
    items.extend(big_old(recs, tot, drive, &skip));
    items.retain_mut(|i| {
        i.paths.retain(|p| p.exists());
        if i.bytes == 0 { i.bytes = i.paths.iter().map(|p| size_of(p)).sum(); }
        i.bytes >= 1 << 20 // crumbs (an "empty" recycle bin is 987 bytes) are noise
    });
    items.sort_by_key(|i| (i.risk as u8, Reverse(i.bytes)));

    let c = vt();
    let (rst, col) = (if c { "\x1b[0m" } else { "" }, |r: Risk| if !c { "" } else { match r { Risk::Safe => "\x1b[32m", Risk::Rebuild => "\x1b[33m", Risk::Ask => "\x1b[31m" } });
    let mut last = None;
    for (n, i) in items.iter().enumerate() {
        if last != Some(i.risk) {
            last = Some(i.risk);
            println!("\n{}{}{rst}", col(i.risk), match i.risk {
                Risk::Safe => "SAFE: Windows and programs recreate this on their own",
                Risk::Rebuild => "CAN BE DELETED: it is recreated, but may run a little slower for a while",
                Risk::Ask => "YOUR CHOICE ONLY: make sure you do not need this",
            });
        }
        let fate = match (i.risk, i.how) { (Risk::Ask, How::Quarantine) => " It goes to quarantine for 30 days and can be restored (rs-du --restore).", (Risk::Ask, _) => " It is deleted permanently.", _ => "" };
        println!("{:>3}. {:>9}  {}\n{:>16}{}{fate}", n + 1, human(i.bytes), i.label, "", i.note);
    }
    let mut sel: Vec<bool> = items.iter().map(|i| all || if safe_only { i.risk == Risk::Safe } else { i.risk != Risk::Ask }).collect();
    let sum = |sel: &[bool]| -> u64 { items.iter().zip(sel).filter(|(_, s)| **s).map(|(i, _)| i.bytes).sum() };
    println!("{}", format!("\nYou can free: {} safely, {} in total ({} places)", human(sum(&sel)), human(items.iter().map(|i| i.bytes).sum()), items.len()));
    let line = |p: &str| { print!("{p}"); let _ = std::io::stdout().flush(); let mut l = String::new(); let _ = std::io::stdin().read_line(&mut l); l };
    if !apply && !ask { println!("{}", "(preview only; --apply deletes the safe items, --apply --all everything, --apply --safe only the green ones)"); return; }
    if !apply {
        select(&line("Enter = clean the safe items (green and yellow), a = everything, 12,13 = safe + these numbers, n = cancel:"), &mut sel);
    }
    if !sel.contains(&true) { if ask { line("Nothing was deleted. Press Enter to close."); } return; }

    purge(drive, false);
    let log = env::var_os("LOCALAPPDATA").map(|l| PathBuf::from(l).join("rs-du")).unwrap_or_default();
    let _ = fs::create_dir_all(&log);
    if let Ok(mut f) = fs::File::create(log.join("clean-log.txt")) {
        for (i, _) in items.iter().zip(&sel).filter(|(_, s)| **s) { for p in &i.paths { let _ = writeln!(f, "{}\t{}", i.bytes, p.display()); } }
    }
    let (stamp, mut seq, mut freed, mut held) = (now(), 0, 0u64, 0u64);
    for (i, _) in items.iter().zip(&sel).filter(|(_, s)| **s) {
        for p in &i.paths {
            match i.how {
                How::Wipe => wipe(p),
                How::Remove => remove(p),
                How::Quarantine => { seq += 1; if !quarantine(p, stamp, seq) { println!("{}", format!("could not remove: {}", p.display())); } }
            }
        }
        let left: u64 = i.paths.iter().map(|p| size_of(p)).sum();
        let gone = i.bytes.saturating_sub(left);
        if i.how == How::Quarantine { held += gone } else { freed += gone }
        if left > 0 && i.how != How::Quarantine { println!("{}", format!("partially left {} (in use by a program or no rights): {}", human(left), i.label)); }
    }
    println!("{}", format!("\nFreed: {}. In quarantine: {} (the space returns in {QUAR_DAYS} days or after rs-du --purge).", human(freed), human(held)));
    println!("{}", format!("List: {}", log.join("clean-log.txt").display()));
    if ask { line("Press Enter to close."); }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_defaults_all_none_and_numbers() {
        let mut s = [true, true, false, false];
        select("", &mut s); assert_eq!(s, [true, true, false, false]);
        select("4", &mut s); assert_eq!(s, [true, true, false, true]);
        select("3, 99,0", &mut s); assert_eq!(s, [true, true, true, true]);
        select("n", &mut s); assert_eq!(s, [false; 4]);
        select("a", &mut s); assert_eq!(s, [true; 4]);
    }

    #[test]
    fn rule_paths_are_absolute() {
        // a missing env var must never turn into a relative path that wipes something in the current dir
        for i in rules() { for p in &i.paths { assert!(p.is_absolute(), "{}", p.display()); } }
    }

    #[test]
    fn manifest_round_trip() {
        let d = env::temp_dir().join("rs-du-test-manifest");
        let _ = fs::create_dir_all(&d);
        let m = d.join("manifest.tsv");
        save(&m, &[(5, r"C:\a".into(), r"C:\.rs-du-quarantine\5\1_a".into())]);
        assert_eq!(parse(&fs::read_to_string(&m).unwrap(), 'c'), vec![(5, PathBuf::from(r"C:\a"), PathBuf::from(r"C:\.rs-du-quarantine\5\1_a"))]);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn manifest_rows_outside_the_quarantine_are_dropped() {
        let t = [r"C:\Windows\System32", r"C:\.rs-du-quarantine\..\Windows", r"D:\.rs-du-quarantine\1\a"].map(|q| format!("0\tC:\\x\t{q}")).join("\n");
        assert!(parse(&t, 'C').is_empty());
    }
}
