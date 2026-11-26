//! What the window remembers between runs, in %APPDATA%\rust-explorer:
//! settings.txt (key=value), session.txt (the open tabs), recent.txt and bookmarks.txt (one path per line).
use std::{env, fs, path::{Path, PathBuf}};

#[derive(Clone, Debug, PartialEq)]
pub struct Prefs {
    pub restore_tabs: bool,  // reopen the last tabs instead of Home
    pub type_filter: bool,   // typing in the list goes to the filter
    pub subfolders: bool,    // the filter looks through the whole tree below (Ctrl+E)
    pub hidden: bool,        // hidden files (Ctrl+H)
    pub folders_first: bool, // folders above files in every sort
    pub recents: bool,       // recent places in the side pane
    pub sort_col: usize,     // 0 name, 1 modified, 2 type, 3 folders, 4 files, 5 size
    pub sort_asc: bool,
    pub collapsed: Vec<u8>,  // side pane sections folded away
    pub zoom: usize,         // step of main.rs ZOOM: row height or picture size (Ctrl+wheel)
    pub folder_sizes: bool,  // count what is inside each folder (Folders / Files / Size columns)
    pub side_pane: bool,     // the side pane is shown (its button at the top left hides it)
    pub smooth_list: bool,   // the GPU-drawn list (glist.rs) instead of the system one, while it is being built
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs { restore_tabs: true, type_filter: true, subfolders: false, hidden: false, folders_first: true, recents: true, sort_col: 0, sort_asc: true, collapsed: vec![], zoom: 0, folder_sizes: true, side_pane: true, smooth_list: false }
    }
}

impl Prefs {
    pub fn parse(s: &str) -> Prefs {
        let mut p = Prefs::default();
        for l in s.lines() {
            let Some((k, v)) = l.split_once('=') else { continue };
            let (k, v) = (k.trim(), v.trim());
            let b = v == "1" || v.eq_ignore_ascii_case("true");
            match k {
                "restore_tabs" => p.restore_tabs = b,
                "type_filter" => p.type_filter = b,
                "subfolders" => p.subfolders = b,
                "hidden" => p.hidden = b,
                "folders_first" => p.folders_first = b,
                "recents" => p.recents = b,
                "sort_col" => p.sort_col = v.parse().unwrap_or(0usize).min(5),
                "sort_asc" => p.sort_asc = b,
                "folder_sizes" => p.folder_sizes = b,
                "side_pane" => p.side_pane = b,
                "smooth_list" => p.smooth_list = b,
                "view_zoom" => p.zoom = v.parse().unwrap_or(0usize), // "zoom" was the older table: ignored
                "collapsed" => p.collapsed = v.split(',').filter_map(|x| x.trim().parse().ok()).collect(),
                _ => {}
            }
        }
        p
    }

    pub fn render(&self) -> String {
        let b = |v: bool| if v { "1" } else { "0" };
        let folded: Vec<String> = self.collapsed.iter().map(|c| c.to_string()).collect();
        format!(
            "restore_tabs={}\ntype_filter={}\nsubfolders={}\nhidden={}\nfolders_first={}\nrecents={}\nsort_col={}\nsort_asc={}\ncollapsed={}\nview_zoom={}\nfolder_sizes={}\nside_pane={}\nsmooth_list={}\n",
            b(self.restore_tabs), b(self.type_filter), b(self.subfolders), b(self.hidden), b(self.folders_first), b(self.recents), self.sort_col, b(self.sort_asc), folded.join(","), self.zoom, b(self.folder_sizes), b(self.side_pane), b(self.smooth_list)
        )
    }
}

pub fn base() -> PathBuf { PathBuf::from(env::var_os("APPDATA").unwrap_or_default()).join("rust-explorer") }

pub fn read(name: &str) -> String { fs::read_to_string(base().join(name)).unwrap_or_default() }

pub fn write(name: &str, s: &str) {
    let d = base();
    let _ = fs::create_dir_all(&d);
    let _ = fs::write(d.join(name), s);
}

pub fn load() -> Prefs { Prefs::parse(&read("settings.txt")) }
pub fn save(p: &Prefs) { write("settings.txt", &p.render()); }

pub fn read_paths(name: &str) -> Vec<PathBuf> { read(name).lines().map(str::trim).filter(|l| !l.is_empty()).map(PathBuf::from).collect() }

pub fn write_paths(name: &str, v: &[PathBuf]) {
    let s: String = v.iter().map(|p| format!("{}\n", p.display())).collect();
    write(name, &s);
}

/// The same place, ignoring case and a trailing backslash ("C:\Users\" = "c:\users").
pub fn same(a: &Path, b: &Path) -> bool {
    let k = |p: &Path| p.to_string_lossy().trim_end_matches('\\').to_lowercase();
    k(a) == k(b)
}

/// Moves (or adds) `p` to the front, keeps at most `max`.
pub fn push_front(v: &mut Vec<PathBuf>, p: PathBuf, max: usize) {
    v.retain(|x| !same(x, &p));
    v.insert(0, p);
    v.truncate(max);
}

/// session.txt: the active tab's number, then one folder per tab.
pub fn render_session(tabs: &[PathBuf], active: usize) -> String {
    let mut s = format!("{active}\n");
    for t in tabs.iter().filter(|t| !t.as_os_str().is_empty()) { s += &format!("{}\n", t.display()); }
    s
}

/// The tabs to reopen: folders that still exist, and which one is active (the same one if it survived, else the first).
pub fn restore(s: &str, exists: impl Fn(&Path) -> bool) -> (Vec<PathBuf>, usize) {
    let mut lines = s.lines().map(str::trim);
    let active: usize = lines.next().and_then(|l| l.parse().ok()).unwrap_or(0);
    let mut tabs = vec![];
    let mut now = 0;
    for (i, l) in lines.filter(|l| !l.is_empty()).enumerate() {
        let p = PathBuf::from(l);
        if !exists(&p) { continue; }
        if i == active { now = tabs.len(); }
        tabs.push(p);
    }
    (tabs, now)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip_and_defaults() {
        assert_eq!(Prefs::parse(""), Prefs::default());
        let p = Prefs { restore_tabs: false, subfolders: true, hidden: true, sort_col: 3, sort_asc: false, collapsed: vec![0, 2], zoom: 11, folder_sizes: false, side_pane: false, ..Prefs::default() };
        assert_eq!(Prefs::parse(&p.render()), p);
        assert_eq!(Prefs::parse("sort_col=9\nnonsense\nhidden = true").sort_col, 5);
        assert!(Prefs::parse("hidden = true").hidden);
    }

    #[test]
    fn recent_list_moves_to_front_without_duplicates() {
        let mut v = vec![PathBuf::from(r"C:\a"), PathBuf::from(r"C:\b")];
        push_front(&mut v, PathBuf::from(r"c:\B\"), 2);
        assert_eq!(v, [PathBuf::from(r"c:\B\"), PathBuf::from(r"C:\a")]);
        push_front(&mut v, PathBuf::from(r"C:\c"), 2);
        assert_eq!(v, [PathBuf::from(r"C:\c"), PathBuf::from(r"c:\B\")]);
    }

    #[test]
    fn session_restores_existing_tabs_and_keeps_the_active_one() {
        let s = render_session(&[PathBuf::from(r"C:\gone"), PathBuf::from(r"C:\a"), PathBuf::from(r"C:\b")], 2);
        let (tabs, active) = restore(&s, |p| !p.ends_with("gone"));
        assert_eq!(tabs, [PathBuf::from(r"C:\a"), PathBuf::from(r"C:\b")]);
        assert_eq!(active, 1);
        let (tabs, active) = restore(&s, |p| p.ends_with("a")); // the active tab is gone: the first one
        assert_eq!((tabs.len(), active), (1, 0));
        assert_eq!(restore("", |_| true), (vec![], 0));
    }
}
