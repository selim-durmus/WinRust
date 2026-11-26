//! Модель интерфейса: страницы, колонки, строки таблиц и их построение из данных.
use crate::{data::*, suspect, ui::*};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Page {
    Processes,
    Performance,
    History,
    Startup,
    Users,
    Details,
    Services,
    Insights,
    Rules,
    Settings,
}

/// Пункты боковой панели: страница, символ значка, подпись.
pub static NAV: std::sync::LazyLock<[(Page, char, &'static str); 10]> = std::sync::LazyLock::new(|| [
    (Page::Processes, '\u{E9D9}', "Processes"),
    (Page::Performance, '\u{E9D2}', "Performance"),
    (Page::History, '\u{E81C}', "App history"),
    (Page::Startup, '\u{EC4A}', "Startup"),
    (Page::Users, '\u{E77B}', "Users"),
    (Page::Details, '\u{EA37}', "Details"),
    (Page::Services, '\u{E9F5}', "Services"),
    (Page::Insights, '\u{E946}', "What's slow"),
    (Page::Rules, '\u{E7BA}', "Rules"),
    (Page::Settings, '\u{E713}', "Settings"),
]);

pub fn page_title(p: Page) -> &'static str {
    NAV.iter().find(|n| n.0 == p).map_or("", |n| n.2)
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Align {
    Left,
    Right,
}

#[derive(Clone, Copy)]
pub struct Col {
    pub title: &'static str,
    pub w: f64, // логические px; 0 — всё оставшееся место
    pub align: Align,
    pub metric: Option<Metric>, // колонка с агрегатом в шапке и тепловой подсветкой
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Metric {
    Cpu,
    Mem,
    Disk,
    Net,
    Gpu,
}

const fn col(title: &'static str, w: f64, align: Align) -> Col {
    Col { title, w, align, metric: None }
}
const fn mcol(title: &'static str, m: Metric) -> Col {
    Col { title, w: 84.0, align: Align::Right, metric: Some(m) }
}

pub fn columns(p: Page) -> Vec<Col> {
    use Align::*;
    use Metric::*;
    match p {
        Page::Processes => vec![
            col("Name", 270.0, Left),
            col("Status", 112.0, Left),
            mcol("CPU", Cpu),
            mcol("Memory", Mem),
            mcol("Disk", Disk),
            mcol("Network", Net),
            mcol("GPU", Gpu),
            col("Suspicious", 140.0, Left),
        ],
        Page::Users => vec![
            col("User", 270.0, Left),
            col("Processes", 112.0, Right),
            mcol("CPU", Cpu),
            mcol("Memory", Mem),
            mcol("Disk", Disk),
            mcol("Network", Net),
            mcol("GPU", Gpu),
        ],
        Page::Details => vec![col("Name", 220.0, Left), col("PID", 70.0, Right), col("Status", 100.0, Left), col("CPU", 70.0, Right), col("Memory", 110.0, Right), col("Disk", 100.0, Right), col("Command line", 0.0, Left)],
        Page::Services => vec![col("Name", 200.0, Left), col("PID", 70.0, Right), col("Description", 360.0, Left), col("Status", 110.0, Left)],
        Page::Startup => vec![col("Name", 240.0, Left), col("Publisher", 200.0, Left), col("Status", 110.0, Left), col("Command", 0.0, Left)],
        Page::History => vec![col("Time", 150.0, Left), col("CPU %", 80.0, Right), col("Memory %", 90.0, Right), col("I/O KB/s", 130.0, Right), col("Network KB/s", 110.0, Right), col("Top consumer", 0.0, Left)],
        Page::Rules => vec![col("Process", 220.0, Left), col("Condition", 280.0, Left), col("Action", 200.0, Left), col("Times triggered", 130.0, Right)],
        _ => vec![],
    }
}

/// Сортировка по умолчанию: (индекс колонки, по убыванию).
pub fn default_sort(p: Page) -> (usize, bool) {
    match p {
        Page::Processes | Page::Users => (2, true), // ЦП
        Page::Details => (3, true),
        Page::History => (0, true),
        _ => (0, false),
    }
}

#[derive(Clone, Default)]
pub struct Row {
    pub key: String,
    pub depth: u8,
    pub expandable: bool,
    pub expanded: bool,
    pub icon_path: Option<String>,
    pub cells: Vec<String>,
    pub heat: Vec<Option<(f64, f64)>>, // (значение, шкала) для колонок-метрик
    pub sortv: Vec<f64>,               // числовые ключи сортировки; NaN — сортировать как текст
    pub sev: u8,
    pub tip: String,
    pub pids: Vec<u32>,
    pub aux: String, // служебное: имя службы, ключ автозагрузки и т.п.
}

pub fn dec(v: f64, digits: usize) -> String {
    format!("{:.*}", digits, v)
}

pub fn fmt_cpu(v: f32) -> String {
    if v < 0.05 { "0%".into() } else { format!("{}%", dec(v as f64, 1)) }
}

pub fn fmt_mem_ru(b: u64) -> String {
    let mb = b as f64 / 1_048_576.0;
    if mb >= 1000.0 { format!("{} MB", group_thousands(mb as u64)) } else if mb >= 100.0 { format!("{} MB", dec(mb, 0)) } else { format!("{} MB", dec(mb, 1)) }
}

pub fn fmt_disk_ru(bps: f64) -> String {
    let mb = bps / 1_048_576.0;
    if mb < 0.05 { "0 MB/s".into() } else { format!("{} MB/s", dec(mb, 1)) }
}

pub fn fmt_net_ru(bps: f64, known: bool) -> String {
    if !known {
        return "—".into();
    }
    let mbit = bps * 8.0 / 1e6;
    if mbit < 0.05 { "0 Mbps".into() } else { format!("{} Mbps", dec(mbit, 1)) }
}

/// Описание/имя для отображения: описание файла, если известно, иначе имя без «.exe».
fn friendly(d: &Data, p: &Proc) -> String {
    if let Some(r) = p.exe.as_deref().and_then(|e| d.res.get(e))
        && !r.desc.is_empty() {
            return r.desc;
        }
    p.name.strip_suffix(".exe").or_else(|| p.name.strip_suffix(".EXE")).unwrap_or(&p.name).to_string()
}

fn svc_display(d: &Data, pid: u32) -> Option<String> {
    let name = d.svc_pids.get(&pid)?.first()?.clone();
    Some(d.svcs.iter().find(|s| s.name == name).map_or(name, |s| s.display.clone()))
}

fn child_name(d: &Data, p: &Proc) -> String {
    if p.name.eq_ignore_ascii_case("svchost.exe")
        && let Some(s) = svc_display(d, p.pid) {
            return format!("Service host: {s}");
        }
    if let Some(t) = &p.title {
        return t.clone();
    }
    friendly(d, p)
}

fn tip_for(procs: &[&Proc], group: bool) -> String {
    let mut by_text: Vec<(u8, String, Vec<u32>)> = vec![];
    for p in procs {
        for r in &p.reasons {
            match by_text.iter_mut().find(|e| e.1 == r.text) {
                Some(e) => e.2.push(p.pid),
                None => by_text.push((r.sev, r.text.clone(), vec![p.pid])),
            }
        }
    }
    if by_text.is_empty() {
        return String::new();
    }
    by_text.sort_by_key(|e| std::cmp::Reverse(e.0));
    let total = by_text.len();
    let mut s = String::from("Why flagged:");
    for (sev, t, pids) in by_text.into_iter().take(6) {
        let who = if pids.len() > 1 { format!(" (processes: {})", pids.len()) } else if group && procs.len() > 1 { format!(" [PID {}]", pids[0]) } else { String::new() };
        s += &format!("\n{} — {t}{who}", suspect::sev_word(sev));
    }
    if total > 6 {
        s += &format!("\n… and {} more reasons", total - 6);
    }
    s
}

fn proc_row(d: &Data, key: String, depth: u8, name: String, members: &[&Proc], expandable: bool, expanded: bool) -> Row {
    let cpu: f32 = members.iter().map(|p| p.cpu).sum();
    let mem: u64 = members.iter().map(|p| p.mem).sum();
    let disk: f64 = members.iter().map(|p| p.disk).sum();
    let net: f64 = members.iter().map(|p| p.net).sum();
    let gpu: f32 = members.iter().map(|p| p.gpu).fold(0.0, f32::max);
    let sev = members.iter().flat_map(|p| p.reasons.iter()).map(|r| r.sev).max().unwrap_or(0);
    let status = if members.iter().all(|p| d.suspended.contains(&p.pid)) {
        "Suspended"
    } else if members.iter().all(|p| d.efficiency.contains(&p.pid)) {
        "Efficiency"
    } else {
        ""
    };
    let etw = d.etw.is_some();
    Row {
        key,
        depth,
        expandable,
        expanded,
        icon_path: members.iter().find_map(|p| p.exe.clone()),
        cells: vec![name, status.into(), fmt_cpu(cpu), fmt_mem_ru(mem), fmt_disk_ru(disk), fmt_net_ru(net, etw), if gpu >= 0.05 { format!("{}%", dec(gpu as f64, 0)) } else { "0%".into() }, suspect::label(sev).to_string()],
        heat: vec![None, None, Some((cpu as f64, 40.0)), Some((mem as f64, 1.5e9)), Some((disk, 5e6)), Some((net, 5e6)), Some((gpu as f64, 40.0)), None],
        sortv: vec![f64::NAN, f64::NAN, cpu as f64, mem as f64, disk, net, gpu as f64, sev as f64],
        sev,
        tip: tip_for(members, members.len() > 1),
        pids: members.iter().map(|p| p.pid).collect(),
        aux: String::new(),
    }
}

pub struct BuildCtx<'a> {
    pub page: Page,
    pub search: &'a str,
    pub expanded: &'a HashSet<String>,
    pub sort: (usize, bool),
}

fn matches(d: &Data, p: &Proc, q: &str) -> bool {
    p.name.to_lowercase().contains(q)
        || p.pid.to_string() == q
        || friendly(d, p).to_lowercase().contains(q)
        || p.exe.as_deref().and_then(|e| d.res.get(e)).is_some_and(|r| r.company.to_lowercase().contains(q))
        || p.title.as_deref().is_some_and(|t| t.to_lowercase().contains(q))
}

pub fn build_rows(d: &Data, c: &BuildCtx) -> Vec<Row> {
    let q = c.search.trim().to_lowercase();
    let mut rows: Vec<Row> = match c.page {
        Page::Processes => {
            let mut groups: HashMap<String, Vec<&Proc>> = HashMap::new();
            for p in &d.procs {
                groups.entry(group_key(&p.name)).or_default().push(p);
            }
            let mut out: Vec<(Row, Vec<Row>)> = vec![];
            for (key, mut members) in groups {
                if !q.is_empty() && !members.iter().any(|p| matches(d, p, &q) || suspect::label(p.reasons.iter().map(|r| r.sev).max().unwrap_or(0)).to_lowercase().contains(&q)) {
                    continue;
                }
                members.sort_by(|a, b| b.cpu.total_cmp(&a.cpu));
                let multi = members.len() > 1;
                let base = if members[0].name.eq_ignore_ascii_case("svchost.exe") { "Service host".to_string() } else { friendly(d, members[0]) };
                let name = if multi { format!("{base} ({})", members.len()) } else if members[0].name.eq_ignore_ascii_case("svchost.exe") { child_name(d, members[0]) } else { members[0].title.clone().filter(|_| false).unwrap_or(base) };
                let expanded = multi && c.expanded.contains(&key);
                let head = proc_row(d, key.clone(), 0, name, &members, multi, expanded);
                let kids: Vec<Row> = if expanded {
                    members.iter().map(|p| proc_row(d, format!("{key}#{}", p.pid), 1, child_name(d, p), &[p], false, false)).collect()
                } else {
                    vec![]
                };
                out.push((head, kids));
            }
            sort_pairs(&mut out, c.sort);
            out.into_iter().flat_map(|(h, k)| std::iter::once(h).chain(k)).collect()
        }
        Page::Users => {
            let mut groups: HashMap<String, Vec<&Proc>> = HashMap::new();
            for p in &d.procs {
                groups.entry(p.user.clone()).or_default().push(p);
            }
            let mut out: Vec<(Row, Vec<Row>)> = vec![];
            for (user, mut members) in groups {
                if !q.is_empty() && !user.to_lowercase().contains(&q) && !members.iter().any(|p| matches(d, p, &q)) {
                    continue;
                }
                members.sort_by(|a, b| b.cpu.total_cmp(&a.cpu));
                let key = format!("user:{user}");
                let expanded = c.expanded.contains(&key);
                let mut head = proc_row(d, key.clone(), 0, user.clone(), &members, true, expanded);
                head.cells[1] = members.len().to_string();
                head.sortv[1] = members.len() as f64;
                head.icon_path = None;
                head.tip = String::new();
                let kids: Vec<Row> = if expanded { members.iter().map(|p| proc_row(d, format!("{key}#{}", p.pid), 1, child_name(d, p), &[p], false, false)).collect() } else { vec![] };
                out.push((head, kids));
            }
            sort_pairs(&mut out, c.sort);
            out.into_iter().flat_map(|(h, k)| std::iter::once(h).chain(k)).collect()
        }
        Page::Details => d
            .procs
            .iter()
            .filter(|p| q.is_empty() || matches(d, p, &q))
            .map(|p| Row {
                key: p.pid.to_string(),
                icon_path: p.exe.clone(),
                cells: vec![p.name.clone(), p.pid.to_string(), (if d.suspended.contains(&p.pid) { "Suspended" } else { "Running" }).into(), fmt_cpu(p.cpu), fmt_mem_ru(p.mem), fmt_disk_ru(p.disk), process_cmd(d, p.pid)],
                sortv: vec![f64::NAN, p.pid as f64, f64::NAN, p.cpu as f64, p.mem as f64, p.disk, f64::NAN],
                pids: vec![p.pid],
                heat: vec![None; 7],
                ..Default::default()
            })
            .collect(),
        Page::Services => d
            .svcs
            .iter()
            .filter(|s| q.is_empty() || s.name.to_lowercase().contains(&q) || s.display.to_lowercase().contains(&q))
            .map(|s| Row {
                key: s.name.clone(),
                cells: vec![s.name.clone(), if s.pid != 0 { s.pid.to_string() } else { String::new() }, s.display.clone(), (if s.running { "Running" } else { "Stopped" }).into()],
                sortv: vec![f64::NAN, s.pid as f64, f64::NAN, f64::NAN],
                pids: vec![s.pid],
                heat: vec![None; 4],
                aux: s.name.clone(),
                ..Default::default()
            })
            .collect(),
        Page::Startup => d
            .startup
            .iter()
            .filter(|s| q.is_empty() || s.name.to_lowercase().contains(&q) || s.command.to_lowercase().contains(&q))
            .map(|s| {
                let exe = exe_from_command(&s.command);
                let company = exe.as_deref().and_then(|e| d.res.get(e)).map(|r| r.company).unwrap_or_default();
                Row {
                    key: s.key(),
                    icon_path: exe,
                    cells: vec![s.name.clone(), company, (if s.enabled { "Enabled" } else { "Disabled" }).into(), s.command.clone()],
                    sortv: vec![f64::NAN; 4],
                    heat: vec![None; 4],
                    tip: format!("Source: {}.{}", s.source_label(), if s.needs_admin() && !d.is_admin { " Administrator rights are needed to change it." } else { "" }),
                    ..Default::default()
                }
            })
            .collect(),
        Page::History => d
            .hist
            .iter()
            .filter(|h| q.is_empty() || h.iter().any(|c| c.to_lowercase().contains(&q)))
            .map(|h| Row {
                key: h[0].clone(),
                cells: h.clone(),
                sortv: vec![f64::NAN, h[1].parse().unwrap_or(0.0), h[2].parse().unwrap_or(0.0), h[3].parse().unwrap_or(0.0), h[4].parse().unwrap_or(0.0), f64::NAN],
                heat: vec![None; 6],
                ..Default::default()
            })
            .collect(),
        Page::Rules => d
            .rules
            .iter()
            .enumerate()
            .map(|(i, r)| Row {
                key: i.to_string(),
                cells: vec![
                    if r.proc == "*" { "any".into() } else { r.proc.clone() },
                    format!("CPU > {:.0}% for over {} s", r.cpu, r.secs),
                    (if r.act == crate::store::RuleAct::Lower { "Lower priority" } else { "Notify" }).into(),
                    r.hits.to_string(),
                ],
                sortv: vec![f64::NAN, f64::NAN, f64::NAN, r.hits as f64],
                heat: vec![None; 4],
                ..Default::default()
            })
            .collect(),
        _ => vec![],
    };
    if !matches!(c.page, Page::Processes | Page::Users) {
        let (ci, desc) = c.sort;
        rows.sort_by(|a, b| {
            let o = cmp_cells(a, b, ci);
            if desc { o.reverse() } else { o }
        });
    }
    rows
}

fn cmp_cells(a: &Row, b: &Row, ci: usize) -> std::cmp::Ordering {
    let (x, y) = (a.sortv.get(ci).copied().unwrap_or(f64::NAN), b.sortv.get(ci).copied().unwrap_or(f64::NAN));
    if x.is_nan() || y.is_nan() {
        a.cells.get(ci).map(|s| s.to_lowercase()).cmp(&b.cells.get(ci).map(|s| s.to_lowercase()))
    } else {
        x.total_cmp(&y)
    }
}

fn sort_pairs(v: &mut [(Row, Vec<Row>)], sort: (usize, bool)) {
    let (ci, desc) = sort;
    let ord = |a: &Row, b: &Row| {
        let o = cmp_cells(a, b, ci);
        if desc { o.reverse() } else { o }
    };
    v.sort_by(|a, b| ord(&a.0, &b.0));
    for (_, kids) in v.iter_mut() {
        kids.sort_by(|a, b| ord(a, b));
    }
}

/// Путь к исполняемому файлу из команды автозагрузки (в кавычках или до первого пробела после «.exe»).
pub fn exe_from_command(cmd: &str) -> Option<String> {
    let c = cmd.trim();
    let path = if let Some(rest) = c.strip_prefix('"') {
        rest.split('"').next()?.to_string()
    } else {
        let l = c.to_lowercase();
        let end = l.find(".exe").map(|i| i + 4).unwrap_or(c.len());
        c[..end].to_string()
    };
    let path = path.replace("%windir%", &std::env::var("WINDIR").unwrap_or_default());
    (!path.is_empty() && path.contains('\\')).then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exe_extraction() {
        assert_eq!(exe_from_command(r#""C:\Program Files\X\x.exe" --silent"#).as_deref(), Some(r"C:\Program Files\X\x.exe"));
        assert_eq!(exe_from_command(r"C:\Tools\a.exe -k b").as_deref(), Some(r"C:\Tools\a.exe"));
        assert_eq!(exe_from_command("cmd /c foo"), None);
    }

    #[test]
    fn number_formats() {
        assert_eq!(fmt_cpu(2.04), "2.0%");
        assert_eq!(fmt_cpu(0.01), "0%");
        assert_eq!(fmt_mem_ru(3_746_000_000), "3,572 MB");
        assert_eq!(fmt_mem_ru(73 * 1_048_576 + 100_000), "73.1 MB");
        assert_eq!(fmt_disk_ru(0.0), "0 MB/s");
        assert_eq!(fmt_net_ru(1.0, false), "—");
    }

    #[test]
    fn every_page_with_table_has_valid_default_sort() {
        for p in [Page::Processes, Page::Users, Page::Details, Page::Services, Page::Startup, Page::History, Page::Rules] {
            assert!(default_sort(p).0 < columns(p).len(), "{p:?}");
        }
    }

    #[test]
    fn users_page_groups_processes_by_account() {
        let mut d = Data::new(crate::store::Settings::default());
        d.refresh();
        let me = d.procs.iter().find(|p| p.pid == std::process::id()).expect("our own process").user.clone();
        assert!(!me.is_empty() && me != "SYSTEM", "our own token must be readable: {me}");
        let mut exp = HashSet::new();
        let rows = build_rows(&d, &BuildCtx { page: Page::Users, search: "", expanded: &exp, sort: default_sort(Page::Users) });
        assert!(rows.iter().all(|r| r.depth == 0 && r.expandable));
        let mine = rows.iter().find(|r| r.cells[0] == me).expect("the current user's row");
        assert!(mine.pids.contains(&std::process::id()));
        exp.insert(mine.key.clone());
        let rows2 = build_rows(&d, &BuildCtx { page: Page::Users, search: "", expanded: &exp, sort: default_sort(Page::Users) });
        assert!(rows2.iter().filter(|r| r.depth == 1).count() == mine.pids.len());
    }

    #[test]
    fn processes_rows_group_and_expand() {
        let mut d = Data::new(crate::store::Settings::default());
        d.refresh();
        let exp: HashSet<String> = HashSet::new();
        let ctx = BuildCtx { page: Page::Processes, search: "", expanded: &exp, sort: default_sort(Page::Processes) };
        let rows = build_rows(&d, &ctx);
        assert!(!rows.is_empty() && rows.iter().all(|r| r.depth == 0));
        let multi = rows.iter().find(|r| r.expandable).expect("a group of several processes");
        let mut exp2 = HashSet::new();
        exp2.insert(multi.key.clone());
        let ctx2 = BuildCtx { page: Page::Processes, search: "", expanded: &exp2, sort: default_sort(Page::Processes) };
        let rows2 = build_rows(&d, &ctx2);
        assert!(rows2.len() > rows.len());
        assert!(rows2.iter().any(|r| r.depth == 1));
        // поиск по PID находит нужную группу
        let pid = std::process::id().to_string();
        let ctx3 = BuildCtx { page: Page::Processes, search: &pid, expanded: &exp, sort: default_sort(Page::Processes) };
        assert!(build_rows(&d, &ctx3).iter().any(|r| r.pids.contains(&std::process::id())));
    }
}
