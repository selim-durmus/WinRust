// rs-du [X:] [--top N] [--depth D] [--logical] [--json]
// rs-du [X:] --clean --ask       numbered list by risk, asks what to clean, waits for Enter at the end (used by the topbar menu)
// rs-du [X:] --clean [--apply [--all|--safe]]   list (or delete: default = green+yellow, --all = everything, --safe = green only)
// rs-du [X:] --restore | --purge   bring quarantined files back | empty the quarantine now (it also empties itself after 30 days)
mod clean;
use winrust_common::mft::*;
// Reads the NTFS MFT (see mft.rs), sums sizes up the parent chain, prints the biggest folders and files. Needs admin (raw volume handle).
// shortcuts: attribute lists / extension records are ignored (very fragmented files are undercounted),
// folder index sizes are not counted; upgrade if numbers drift from Explorer.
use std::{collections::{HashMap, HashSet}, env, fs, io::Write, path::{Path, PathBuf}};

/// Bytes under `p`, not following symlinks/junctions.
fn dir_size(p: &Path) -> u64 {
    let Ok(rd) = fs::read_dir(p) else { return 0 };
    rd.flatten().map(|e| match e.file_type() { Ok(t) if t.is_dir() => dir_size(&e.path()), Ok(t) if t.is_symlink() => 0, _ => e.metadata().map_or(0, |m| m.len()) }).sum()
}

fn wipe(p: &Path) {
    if let Ok(rd) = fs::read_dir(p) {
        for e in rd.flatten() {
            let q = e.path();
            let _ = if e.file_type().is_ok_and(|t| t.is_dir()) { fs::remove_dir_all(&q) } else { fs::remove_file(&q) }; // locked/in-use: skipped
        }
    }
}

fn main() {
    let a: Vec<String> = env::args().skip(1).collect();
    let flag = |f: &str| a.iter().any(|x| x == f);
    let val = |f: &str, d: usize| a.iter().position(|x| x == f).and_then(|i| a.get(i + 1)?.parse().ok()).unwrap_or(d);
    let drive = a.first().filter(|x| x.len() == 2 && x.ends_with(':')).and_then(|x| x.chars().next()).unwrap_or('C').to_ascii_uppercase();
    let (top, depth, logical, json) = (val("--top", 20), val("--depth", 2), flag("--logical"), flag("--json"));
    if flag("--restore") { return clean::restore(drive); }
    if flag("--purge") { return clean::purge(drive, true); }
    let t = std::time::Instant::now();
    let recs = scan(drive).unwrap_or_else(|e| { eprintln!("{e}"); std::process::exit(1) });
    let tot = totals(&recs, logical);
    if flag("--clean") { clean::clean(&recs, &tot, drive, flag("--apply"), flag("--ask"), flag("--all"), flag("--safe")); return; }
    let mut dirs: Vec<(u32, u64)> = vec![];
    let mut files: Vec<(u32, u64)> = vec![];
    for (&id, &s) in &tot {
        match recs.get(&id) { Some(r) if r.dir && id != ROOT => dirs.push((id, s)), Some(_) if id != ROOT => files.push((id, s)), _ => {} }
    }
    dirs.retain(|&(id, _)| path_of(&recs, id, drive).1 <= depth);
    dirs.sort_by_key(|&(_, s)| std::cmp::Reverse(s));
    files.sort_by_key(|&(_, s)| std::cmp::Reverse(s));
    let rows = |v: &[(u32, u64)]| -> Vec<(String, u64)> { v.iter().take(top).map(|&(id, s)| (path_of(&recs, id, drive).0, s)).collect() };
    let (d, f) = (rows(&dirs), rows(&files));
    if json {
        let j = |v: &[(String, u64)]| v.iter().map(|(p, s)| format!("{{\"path\":\"{}\",\"bytes\":{s}}}", p.replace('\\', "\\\\").replace('"', "\\\""))).collect::<Vec<_>>().join(",");
        println!("{{\"drive\":\"{drive}\",\"folders\":[{}],\"files\":[{}]}}", j(&d), j(&f));
    } else {
        println!("{drive}: {} records, {} total ({}), {:.2}s", recs.len(), human(tot.get(&ROOT).copied().unwrap_or(0)), if logical { "logical" } else { "on disk" }, t.elapsed().as_secs_f32());
        println!("\nFolders (depth <= {depth}):");
        for (p, s) in &d { println!("{:>10}  {p}", human(*s)); }
        println!("\nFiles:");
        for (p, s) in &f { println!("{:>10}  {p}", human(*s)); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> Vec<u8> {
        let mut b = vec![0u8; 1024];
        b[..4].copy_from_slice(b"FILE");
        b[4] = 0x30; b[6] = 3; // usa at 0x30, 3 entries
        b[0x30] = 1; // seq
        b[510] = 1; b[1022] = 1;
        b[0x14] = 0x38; b[0x16] = 1; // first attr, in use
        let mut o = 0x38;
        // $FILE_NAME resident: parent=5, name "ab", Win32
        b[o..o + 4].copy_from_slice(&0x30u32.to_le_bytes());
        b[o + 4] = 0x70; b[o + 0x14] = 0x18;
        let c = o + 0x18;
        b[c] = 5; b[c + 0x40] = 2; b[c + 0x41] = 1;
        b[c + 0x42..c + 0x46].copy_from_slice(&[b'a', 0, b'b', 0]);
        o += 0x70;
        // $DATA non-resident: alloc 8192, real 5000
        b[o..o + 4].copy_from_slice(&0x80u32.to_le_bytes());
        b[o + 4] = 0x48; b[o + 8] = 1;
        b[o + 0x28..o + 0x30].copy_from_slice(&8192u64.to_le_bytes());
        b[o + 0x30..o + 0x38].copy_from_slice(&5000u64.to_le_bytes());
        o += 0x48;
        b[o..o + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        b[0x18] = (o + 8) as u8; b[0x19] = ((o + 8) >> 8) as u8;
        b
    }

    #[test]
    fn parses_name_parent_sizes() {
        let (_, r) = parse(&mut record()).unwrap();
        assert_eq!((r.name.as_str(), r.parent, r.dir, r.alloc, r.logical), ("ab", 5, false, 8192, 5000));
    }

    #[test]
    fn torn_or_unused_records_are_rejected() {
        let mut b = record();
        b[510] = 9;
        assert!(parse(&mut b).is_none());
        let mut b = record();
        b[0x16] = 0;
        assert!(parse(&mut b).is_none());
    }

    #[test]
    fn wipe_empties_dir_but_keeps_it() {
        let d = env::temp_dir().join("rs-du-test-wipe");
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("a/b")).unwrap();
        fs::write(d.join("a/b/f"), [0u8; 100]).unwrap();
        fs::write(d.join("g"), [0u8; 50]).unwrap();
        assert_eq!(dir_size(&d), 150);
        wipe(&d);
        assert!(d.is_dir() && dir_size(&d) == 0 && fs::read_dir(&d).unwrap().count() == 0);
        fs::remove_dir(&d).unwrap();
    }

    #[test]
    fn decodes_data_runs_with_negative_offsets() {
        let mut b = vec![0u8; 256];
        b[0x14] = 0x38;
        b[0x38..0x3C].copy_from_slice(&0x80u32.to_le_bytes());
        b[0x3C] = 0x48; b[0x40] = 1; b[0x58] = 0x40; // $DATA, non-resident, runlist at +0x40
        b[0x78..0x7F].copy_from_slice(&[0x21, 0x18, 0x34, 0x12, 0x11, 0x08, 0xFE]);
        assert_eq!(data_runs(&b).unwrap(), vec![(0x1234, 24), (0x1232, 8)]);
    }
}
