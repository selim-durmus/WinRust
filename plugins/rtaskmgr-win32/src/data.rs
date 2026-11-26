//! Данные системы: процессы, службы, автозагрузка, история, правила, показатели производительности.
//! Не знает ничего об интерфейсе.
use crate::{gpu, hw, perf, net, res::ResCache, startup, store, suspect, sys};
use std::{
    collections::{HashMap, VecDeque},
    time::Instant,
};
use store::{Rule, RuleAct};
use sysinfo::{Networks, Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

pub const CAP: usize = 60; // точек на графиках (≈ минута)
const EMA: f32 = 2.0 / 31.0; // сглаживание ≈ среднее за 30 замеров
pub const CRITICAL: [&str; 8] = ["system", "registry", "smss.exe", "csrss.exe", "wininit.exe", "winlogon.exe", "services.exe", "lsass.exe"];

#[derive(Clone)]
pub struct Proc {
    pub pid: u32,
    pub ppid: u32,
    pub name: String,
    pub exe: Option<String>,
    pub cpu: f32, // % от всех ядер
    pub avg: f32, // среднее за ~30 с
    pub mem: u64,
    pub disk: f64, // байт/с (чтение+запись)
    pub net: f64,  // байт/с, только при включённом ETW
    pub gpu: f32,  // %
    pub reasons: Vec<suspect::Reason>,
    pub title: Option<String>, // заголовок видимого окна (если есть)
    pub user: String,          // учётная запись; для процессов с недоступным токеном — «СИСТЕМА»
}

#[derive(Default)]
pub struct Series {
    pub cpu: VecDeque<f32>,
    pub mem: VecDeque<f32>,
    pub disk: VecDeque<f32>, // активность, %
    pub rd: VecDeque<f32>,   // МБ/с
    pub wr: VecDeque<f32>,
    pub rx: VecDeque<f32>, // Кбит/с
    pub tx: VecDeque<f32>,
    pub gpu: VecDeque<f32>, // общая загрузка (максимум по адаптерам)
    pub cores: Vec<VecDeque<f32>>,
}

pub fn push(q: &mut VecDeque<f32>, v: f32) {
    if q.len() == CAP {
        q.pop_front();
    }
    q.push_back(v);
}

/// Ряды одного видеоадаптера.
#[derive(Default)]
pub struct GpuSeries {
    pub total: VecDeque<f32>,
    pub g3d: VecDeque<f32>,
    pub gcopy: VecDeque<f32>,
    pub genc: VecDeque<f32>,
    pub gdec: VecDeque<f32>,
    pub ded: VecDeque<f32>, // МБ
    pub shr: VecDeque<f32>, // МБ
}

pub struct GpuState {
    pub adapter: gpu::Adapter,
    pub reg: hw::GpuReg,
    pub util: f32,
    pub ded_used: u64,
    pub shr_used: u64,
    pub temp: Option<f32>,
    pub series: GpuSeries,
}

/// Один физический диск: сведения и ряды.
pub struct DiskState {
    pub number: u32,
    pub letters: String,
    pub info: hw::DiskInfo,
    pub formatted: u64,
    pub active: f32,
    pub rd: f64,
    pub wr: f64,
    pub ms: f32,
    pub s_active: VecDeque<f32>,
    pub s_rd: VecDeque<f32>, // МБ/с
    pub s_wr: VecDeque<f32>,
}

/// Один сетевой адаптер.
pub struct NicState {
    pub name: String,
    pub rx: f64, // байт/с
    pub tx: f64,
    pub bw: f64, // бит/с
    pub s_rx: VecDeque<f32>, // Кбит/с
    pub s_tx: VecDeque<f32>,
}

#[derive(Default)]
struct HistAcc {
    n: u32,
    cpu: f32,
    mem: f32,
    disk: f32,
    net: f32,
}

pub struct Data {
    pub sys: System,
    nets: Networks,
    pub procs: Vec<Proc>,
    ema: HashMap<u32, f32>,
    pub svcs: Vec<sys::Svc>,
    pub svc_pids: HashMap<u32, Vec<String>>,
    pub startup: Vec<startup::Item>,
    pub hist: Vec<Vec<String>>,
    pub series: Series,
    pub rules: Vec<Rule>,
    rule_cnt: HashMap<(usize, u32), u32>,
    acc: HistAcc,
    pub cfg: store::Settings,
    sig: suspect::SigCache,
    susp_info: HashMap<u32, (u8, u32)>,
    pub net: HashMap<u32, f64>,
    pub etw: Option<net::Session>,
    pub perf: Option<perf::Perf>,
    pub sample: perf::Sample,
    pub sysinfo: perf::SysInfo,
    pub meminfo: perf::MemInfo,
    pub res: ResCache,
    pub cpu_total: f32,
    pub mem_pct: f32,
    pub ticks: u32,
    prev: Instant,
    pub is_admin: bool,
    pub efficiency: std::collections::HashSet<u32>, // процессы, которым мы включили режим эффективности
    pub suspended: std::collections::HashSet<u32>,  // процессы, которые мы приостановили
    pub disk_total: u64,
    pub hw_mem: hw::MemHw,
    pub gpus: Vec<GpuState>,
    pub disks: Vec<DiskState>,
    pub nics: Vec<NicState>,
    nvml: Option<hw::Nvml>,
    nvml_tried: bool,
    user_cache: HashMap<u32, String>,
    /// Температуру GPU читаем, пока открыта страница GPU (или всегда, если включено в параметрах):
    /// опрос драйвера может будить дискретную видеокарту ноутбука.
    pub want_gpu_temp: bool,
}

pub fn timestamp() -> String {
    let (y, mo, d, h, mi, _) = sys::local_time();
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}")
}

pub fn group_key(name: &str) -> String {
    name.to_lowercase()
}

impl Data {
    pub fn new(cfg: store::Settings) -> Data {
        let is_admin = sys::is_admin();
        let mut d = Data::new_inner(cfg, is_admin);
        d.gpus = gpu::adapters()
            .into_iter()
            .map(|a| GpuState { reg: hw::gpu_registry(&a.name), adapter: a, util: 0.0, ded_used: 0, shr_used: 0, temp: None, series: GpuSeries::default() })
            .collect();
        d
    }

    fn new_inner(cfg: store::Settings, is_admin: bool) -> Data {
        Data {
            sys: System::new().expect("sysinfo"),
            nets: Networks::new_with_refreshed_list().expect("network interfaces"),
            procs: vec![],
            ema: HashMap::new(),
            svcs: vec![],
            svc_pids: HashMap::new(),
            startup: vec![],
            hist: vec![],
            series: Series::default(),
            rules: cfg.rules.clone(),
            rule_cnt: HashMap::new(),
            acc: HistAcc::default(),
            etw: if is_admin && cfg.etw { net::start() } else { None },
            cfg,
            sig: suspect::SigCache::new(),
            susp_info: HashMap::new(),
            net: HashMap::new(),
            perf: perf::Perf::new(),
            sample: perf::Sample::default(),
            sysinfo: perf::static_info(),
            meminfo: perf::MemInfo::default(),
            res: ResCache::new(),
            cpu_total: 0.0,
            mem_pct: 0.0,
            ticks: 0,
            prev: Instant::now(),
            is_admin,
            efficiency: Default::default(),
            suspended: Default::default(),
            disk_total: 0,
            hw_mem: hw::memory_hw(),
            gpus: vec![],
            disks: vec![],
            nics: vec![],
            nvml: None,
            nvml_tried: false,
            user_cache: HashMap::new(),
            want_gpu_temp: false,
        }
    }

    pub fn load_startup(&mut self) {
        self.startup = startup::list();
    }

    pub fn load_history(&mut self) {
        self.hist = store::read_history(3000);
    }

    /// Один такт обновления. Возвращает сообщения от правил (их показывает интерфейс).
    pub fn refresh(&mut self) -> Vec<String> {
        let dt = self.prev.elapsed().as_secs_f64().clamp(0.3, 5.0);
        self.prev = Instant::now();
        self.ticks += 1;
        let first = self.ticks == 1; // первый замер у sysinfo неточный — не рисуем его
        self.sys.refresh_cpu_usage();
        self.sys.refresh_memory();
        let kind = ProcessRefreshKind::nothing()
            .with_cpu()
            .with_memory()
            .with_disk_usage()
            .with_exe(UpdateKind::OnlyIfNotSet)
            .with_cmd(UpdateKind::OnlyIfNotSet);
        self.sys.refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
        self.nets.refresh(true);

        self.cpu_total = if first { 0.0 } else { self.sys.global_cpu_usage() };
        let (used, total) = (self.sys.used_memory(), self.sys.total_memory().max(1));
        self.mem_pct = (100.0 * used as f64 / total as f64) as f32;
        let cores = self.sys.cpus().len().max(1) as f32;
        let cur: HashMap<u32, f32> = self.sys.processes().values().map(|p| (p.pid().as_u32(), p.cpu_usage() / cores)).collect();
        self.ema.retain(|k, _| cur.contains_key(k));
        for (k, v) in &cur {
            let e = self.ema.entry(*k).or_insert(*v);
            *e += EMA * (*v - *e);
        }
        if self.etw.is_some() {
            self.net = net::drain().into_iter().map(|(pid, (rx, tx))| (pid, (rx + tx) as f64 / dt)).collect();
        }
        self.sample = self.perf.as_mut().map(|p| p.sample()).unwrap_or_default();
        self.meminfo = perf::mem_info();
        if self.want_gpu_temp || self.cfg.gpu_temp_always {
            for g in self.gpus.iter_mut() {
                g.temp = gpu::temperature(g.adapter.luid).or_else(|| {
                    // запасной путь для NVIDIA, если драйвер не отдаёт температуру через D3DKMT
                    if g.adapter.vendor == 0x10DE {
                        if !self.nvml_tried {
                            self.nvml_tried = true;
                            self.nvml = hw::Nvml::new();
                        }
                        self.nvml.as_ref().and_then(|n| n.temp()).map(|t| t as f32)
                    } else {
                        None
                    }
                });
            }
        } else {
            for g in self.gpus.iter_mut() {
                g.temp = None;
            }
        }
        if self.ticks % 30 == 1 {
            self.disk_total = disk_capacity();
        }
        if self.ticks % 5 == 1 {
            self.svcs = sys::services();
            self.svc_pids.clear();
            for s in self.svcs.iter().filter(|s| s.pid != 0) {
                self.svc_pids.entry(s.pid).or_default().push(s.name.clone());
            }
        }
        let titles = if self.ticks % 2 == 1 { Some(sys::window_titles()) } else { None };

        // ---- подозрительность ----
        let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
        let mut reasons: HashMap<u32, Vec<suspect::Reason>> = HashMap::new();
        for p in self.sys.processes().values() {
            let pid = p.pid().as_u32();
            let exe = p.exe().map(|e| e.display().to_string()).filter(|e| !e.is_empty());
            let parent = p.parent().and_then(|pp| self.sys.process(pp)).map(|x| x.name().to_string_lossy().into_owned());
            let entry = self.susp_info.entry(pid).or_insert((0, 0));
            if self.ticks >= entry.1 {
                // «файла нет» подтверждаем двумя проверками подряд (через 3 такта), иначе проверяем раз в минуту
                let missing = exe.as_deref().is_some_and(suspect::file_missing);
                entry.0 = if missing { entry.0.saturating_add(1) } else { 0 };
                entry.1 = self.ticks + if entry.0 == 1 { 3 } else { 60 };
            }
            let exists = entry.0 < 2;
            let sig = match exe.as_deref() {
                Some(e) if !e.to_lowercase().starts_with(&windir.to_lowercase()) => self.sig.get(e),
                _ => suspect::Sig::Unknown,
            };
            let name = p.name().to_string_lossy().into_owned();
            let r = suspect::assess(&suspect::Facts { name: &name, exe: exe.as_deref(), parent: parent.as_deref(), exists, sig, windir: &windir });
            if !r.is_empty() {
                reasons.insert(pid, r);
            }
        }
        self.susp_info.retain(|k, _| cur.contains_key(k));

        // ---- список процессов ----
        let old_titles: HashMap<u32, String> = self.procs.iter().filter_map(|p| p.title.clone().map(|t| (p.pid, t))).collect();
        let user_cache = &mut self.user_cache;
        let procs: Vec<Proc> = self
            .sys
            .processes()
            .values()
            .map(|p| {
                let pid = p.pid().as_u32();
                let d = p.disk_usage();
                Proc {
                    pid,
                    ppid: p.parent().map_or(0, |x| x.as_u32()),
                    name: p.name().to_string_lossy().into_owned(),
                    exe: p.exe().map(|e| e.display().to_string()).filter(|e| !e.is_empty()),
                    cpu: cur.get(&pid).copied().unwrap_or(0.0),
                    avg: self.ema.get(&pid).copied().unwrap_or(0.0),
                    mem: p.memory(),
                    disk: if first { 0.0 } else { (d.read_bytes + d.written_bytes) as f64 / dt },
                    net: self.net.get(&pid).copied().unwrap_or(0.0),
                    gpu: self.sample.gpu_by_pid.get(&pid).copied().unwrap_or(0.0),
                    reasons: reasons.get(&pid).cloned().unwrap_or_default(),
                    user: user_cache.entry(pid).or_insert_with(|| sys::process_user(pid).unwrap_or_else(|| "SYSTEM".into())).clone(),
                    title: match &titles {
                        Some(t) => t.get(&pid).cloned(),
                        None => old_titles.get(&pid).cloned(),
                    },
                }
            })
            .collect();
        self.procs = procs;
        self.user_cache.retain(|k, _| cur.contains_key(k));
        self.efficiency.retain(|p| cur.contains_key(p));
        self.suspended.retain(|p| cur.contains_key(p));

        // ---- ряды для графиков ----
        let (rx, tx) = self.sample.net.as_ref().map_or_else(
            || {
                let r = self.nets.iter().fold((0u64, 0u64), |a, (_, n)| (a.0 + n.received(), a.1 + n.transmitted()));
                (r.0 as f64 / dt, r.1 as f64 / dt)
            },
            |n| (n.rx, n.tx),
        );
        let per_core: Vec<f32> = self.sys.cpus().iter().map(|c| c.usage()).collect();
        if !first {
            let s = &mut self.series;
            push(&mut s.cpu, self.cpu_total);
            push(&mut s.mem, self.mem_pct);
            push(&mut s.disk, self.sample.disk_active);
            push(&mut s.rd, (self.sample.disk_read / 1_048_576.0) as f32);
            push(&mut s.wr, (self.sample.disk_write / 1_048_576.0) as f32);
            push(&mut s.rx, (rx * 8.0 / 1000.0) as f32);
            push(&mut s.tx, (tx * 8.0 / 1000.0) as f32);
            push(&mut s.gpu, self.sample.gpu_total);
            let _ = &s;
            s.cores.resize_with(per_core.len(), VecDeque::new);
            for (q, v) in s.cores.iter_mut().zip(per_core) {
                push(q, v);
            }
        }

        // ---- адаптеры, диски, сетевые интерфейсы: состояния и ряды ----
        if !first {
            for g in self.gpus.iter_mut() {
                let smp = self.sample.gpus.get(&g.adapter.luid).cloned().unwrap_or_default();
                let ty = |k: &str| smp.types.get(k).copied().unwrap_or(0.0);
                g.util = smp.total;
                g.ded_used = smp.ded;
                g.shr_used = smp.shr;
                let s = &mut g.series;
                push(&mut s.total, smp.total);
                push(&mut s.g3d, ty("3D"));
                push(&mut s.gcopy, ty("Copy"));
                push(&mut s.genc, ty("VideoEncode"));
                push(&mut s.gdec, ty("VideoDecode"));
                push(&mut s.ded, (smp.ded >> 20) as f32);
                push(&mut s.shr, (smp.shr >> 20) as f32);
            }
            for ds in &self.sample.disks {
                let i = match self.disks.iter().position(|d| d.number == ds.number) {
                    Some(i) => i,
                    None => {
                        self.disks.push(DiskState {
                            number: ds.number,
                            letters: ds.letters.clone(),
                            info: hw::disk_info_for(ds.number),
                            formatted: hw::volumes_total(&ds.letters),
                            active: 0.0,
                            rd: 0.0,
                            wr: 0.0,
                            ms: 0.0,
                            s_active: Default::default(),
                            s_rd: Default::default(),
                            s_wr: Default::default(),
                        });
                        self.disks.len() - 1
                    }
                };
                let d = &mut self.disks[i];
                (d.letters, d.active, d.rd, d.wr, d.ms) = (ds.letters.clone(), ds.active, ds.rd, ds.wr, ds.ms);
                push(&mut d.s_active, ds.active);
                push(&mut d.s_rd, (ds.rd / 1_048_576.0) as f32);
                push(&mut d.s_wr, (ds.wr / 1_048_576.0) as f32);
            }
            self.disks.sort_by_key(|d| d.number);
            for n in &self.sample.nics {
                let i = match self.nics.iter().position(|x| x.name == n.name) {
                    Some(i) => i,
                    None => {
                        self.nics.push(NicState { name: n.name.clone(), rx: 0.0, tx: 0.0, bw: 0.0, s_rx: Default::default(), s_tx: Default::default() });
                        self.nics.len() - 1
                    }
                };
                let x = &mut self.nics[i];
                (x.rx, x.tx, x.bw) = (n.rx, n.tx, n.bw);
                push(&mut x.s_rx, (n.rx * 8.0 / 1000.0) as f32);
                push(&mut x.s_tx, (n.tx * 8.0 / 1000.0) as f32);
            }
            // адаптеры, исчезнувшие из счётчиков (отключили кабель, выключили Wi-Fi), убираем
            let alive: std::collections::HashSet<&String> = self.sample.nics.iter().map(|n| &n.name).collect();
            self.nics.retain(|n| alive.contains(&n.name));
        }

        // ---- правила ----
        let mut notes: Vec<String> = vec![];
        for ri in 0..self.rules.len() {
            let rule = self.rules[ri].clone();
            for p in self.procs.iter().filter(|p| rule.proc == "*" || p.name.to_lowercase() == rule.proc) {
                if p.cpu > rule.cpu && !CRITICAL.contains(&p.name.to_lowercase().as_str()) {
                    let c = self.rule_cnt.entry((ri, p.pid)).or_insert(0);
                    *c += 1;
                    if *c >= rule.secs {
                        *c = 0;
                        match rule.act {
                            RuleAct::Lower => {
                                if sys::priority_name(p.pid) != sys::PRIORITIES[1].0 && sys::set_priority(p.pid, sys::PRIORITIES[1].1) {
                                    notes.push(format!("Rule: '{}' (PID {}) used CPU > {:.0}% - priority lowered", p.name, p.pid, rule.cpu));
                                    self.rules[ri].hits += 1;
                                }
                            }
                            RuleAct::Notify => {
                                notes.push(format!("'{}' (PID {}) has used more than {:.0}% CPU for {} s", p.name, p.pid, rule.cpu, rule.secs));
                                self.rules[ri].hits += 1;
                            }
                        }
                    }
                } else {
                    self.rule_cnt.remove(&(ri, p.pid));
                }
            }
        }
        self.rule_cnt.retain(|(_, pid), _| cur.contains_key(pid));

        // ---- история: одна запись в минуту ----
        if !first {
            let io = ((self.sample.disk_read + self.sample.disk_write) / 1024.0) as f32;
            let top = if self.acc.n + 1 >= 60 { self.top_consumer() } else { String::new() };
            let a = &mut self.acc;
            (a.n, a.cpu, a.mem, a.disk, a.net) = (a.n + 1, a.cpu + self.cpu_total, a.mem + self.mem_pct, a.disk + io, a.net + ((rx + tx) / 1024.0) as f32);
            if a.n >= 60 {
                let n = a.n as f32;
                store::append_history(&format!("{};{:.0};{:.0};{:.0};{:.0};{}", timestamp(), a.cpu / n, a.mem / n, a.disk / n, a.net / n, top));
                self.acc = HistAcc::default();
            }
        }
        notes
    }

    /// Главный потребитель ЦП по среднему за 30 с: «имя (N%)».
    fn top_consumer(&self) -> String {
        let mut g: HashMap<String, (String, f32)> = HashMap::new();
        for p in &self.procs {
            let e = g.entry(group_key(&p.name)).or_insert((p.name.clone(), 0.0));
            e.1 += p.avg;
        }
        g.into_values().max_by(|a, b| a.1.total_cmp(&b.1)).map_or(String::new(), |(n, v)| format!("{n} ({v:.0}%)"))
    }

    /// Подсказки «что сейчас тормозит» по текущим данным.
    pub fn hints(&self) -> Vec<String> {
        let mut out = vec![];
        let mut groups: HashMap<String, Vec<&Proc>> = HashMap::new();
        for p in &self.procs {
            groups.entry(group_key(&p.name)).or_default().push(p);
        }
        let disp = |g: &[&Proc]| if g.len() > 1 { format!("{} ({} pcs)", g[0].name, g.len()) } else { g[0].name.clone() };
        let avg = |g: &Vec<&Proc>| g.iter().map(|p| p.avg).sum::<f32>();
        let mut gs: Vec<&Vec<&Proc>> = groups.values().collect();
        gs.sort_by(|a, b| avg(b).total_cmp(&avg(a)));
        for g in gs.iter().take(3) {
            let (a, now): (f32, f32) = (avg(g), g.iter().map(|p| p.cpu).sum());
            if a >= 8.0 {
                let mut t = format!("CPU: '{}' averages {a:.0}% over the last 30 s (now {now:.0}%).", disp(g));
                if g[0].name.eq_ignore_ascii_case("svchost.exe") {
                    let top = g.iter().max_by(|x, y| x.avg.total_cmp(&y.avg)).unwrap();
                    if let Some(s) = self.svc_pids.get(&top.pid) {
                        t += &format!(" The busiest svchost (PID {}) hosts services: {}.", top.pid, s.iter().take(5).cloned().collect::<Vec<_>>().join(", "));
                    }
                }
                out.push(t);
            }
        }
        if self.mem_pct >= 80.0 {
            gs.sort_by_key(|g| std::cmp::Reverse(g.iter().map(|p| p.mem).sum::<u64>()));
            let top: Vec<String> = gs.iter().take(3).map(|g| format!("{} — {}", disp(g), crate::ui::fmt_mem(g.iter().map(|p| p.mem).sum()))).collect();
            out.push(format!("Memory is {:.0}% used. Biggest users: {}.", self.mem_pct, top.join("; ")));
        }
        let io = self.sample.disk_read + self.sample.disk_write;
        if io > 150.0 * 1_048_576.0 || self.sample.disk_active > 90.0 {
            out.push(format!("Disk is {:.0}% busy (read+write {:.0} MB/s).", self.sample.disk_active, io / 1_048_576.0));
        }
        if self.etw.is_some() {
            gs.sort_by(|a, b| b.iter().map(|p| p.net).sum::<f64>().total_cmp(&a.iter().map(|p| p.net).sum::<f64>()));
            if let Some(g) = gs.first() {
                let n: f64 = g.iter().map(|p| p.net).sum();
                if n > 1_000_000.0 {
                    out.push(format!("Network: '{}' moves {:.1} MB/s - more than any other.", disp(g), n / 1e6));
                }
            }
        }
        let high: Vec<String> = gs.iter().filter(|g| g.iter().any(|p| p.reasons.iter().any(|r| r.sev == 3))).map(|g| disp(g)).take(5).collect();
        if !high.is_empty() {
            out.push(format!("Suspicious processes (high severity): {}. Hover the label in the 'Suspicious' column to see the reason.", high.join(", ")));
        }
        if self.cpu_total > 90.0 {
            out.push("CPU is almost fully used - sort processes by CPU to see the culprit.".into());
        }
        if out.is_empty() {
            out.push("All good: no process uses noticeably more than usual.".into());
        }
        out
    }
}

/// Размер системного диска, байт.
fn disk_capacity() -> u64 {
    use sysinfo::Disks;
    Disks::new_with_refreshed_list()
        .map(|d| (&d).into_iter().find(|x| x.mount_point().to_string_lossy().to_uppercase().starts_with("C:")).map_or(0, |x| x.total_space()))
        .unwrap_or(0)
}

pub fn process_cmd(d: &Data, pid: u32) -> String {
    d.sys.process(Pid::from_u32(pid)).map(|p| p.cmd().iter().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>().join(" ")).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn series_keeps_only_cap_points() {
        let mut q = VecDeque::new();
        for i in 0..(CAP + 30) {
            push(&mut q, i as f32);
        }
        assert_eq!(q.len(), CAP);
        assert_eq!(*q.front().unwrap(), 30.0);
    }

    #[test]
    fn refresh_fills_processes_and_series() {
        let mut d = Data::new(store::Settings::default());
        d.refresh();
        std::thread::sleep(std::time::Duration::from_millis(1200));
        d.refresh();
        assert!(d.procs.len() > 20);
        assert!(d.procs.iter().any(|p| p.pid == std::process::id()));
        assert_eq!(d.series.cpu.len(), 1); // первый замер отбрасывается
        assert!(d.meminfo.processes > 0);
        assert!(!d.hints().is_empty());
    }
}
