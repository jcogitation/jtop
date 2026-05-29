use crossterm::{
    cursor, execute,
    terminal::{self, ClearType},
    event::{self, Event, KeyCode},
};
use std::{
    collections::HashMap,
    env,
    fs,
    io::{self, Write},
    time::{Duration, Instant},
};

// ── Terminal Guard: guarantees cleanup on exit, panic, or signal ──────
struct TerminalGuard {
    raw_enabled: bool,
    alt_screen_enabled: bool,
}

impl TerminalGuard {
    fn new() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let mut guard = Self { raw_enabled: true, alt_screen_enabled: false };
        execute!(io::stdout(), terminal::EnterAlternateScreen, cursor::Hide)?;
        guard.alt_screen_enabled = true;
        Ok(guard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.alt_screen_enabled {
            let _ = execute!(io::stdout(), terminal::LeaveAlternateScreen, cursor::Show);
        }
        if self.raw_enabled {
            let _ = terminal::disable_raw_mode();
        }
    }
}

// ── Colour helpers and memory formatting ───────────────────────────────
const YELLOW_BRIGHT: (u8, u8, u8) = (255, 220, 0);
const BLUE_BRIGHT:   (u8, u8, u8) = (0, 128, 255);
const RED_DESAT:     (u8, u8, u8) = (255, 60, 60);
const INDIGO_DESAT:  (u8, u8, u8) = (111, 81, 161);
const WHITE_DIMMER:  (u8, u8, u8) = (200, 200, 200);
const WHITE_PURE:    (u8, u8, u8) = (255, 255, 255);
const CYAN_BRIGHT:   (u8, u8, u8) = (0, 255, 255);

fn format_memory(kib: u64) -> String {
    if kib == 0 { return "0 KiB".to_string(); }
    let mib = kib as f64 / 1024.0;
    if mib >= 1000.0 { format!("{} MiB", mib.round() as u64) }
    else if mib >= 100.0 { format!("{} MiB", (mib.round() as u64)) }
    else if mib >= 10.0 { format!("{:.1} MiB", mib) }
    else if mib >= 1.0 { format!("{:.2} MiB", mib) }
    else { format!("{} KiB", kib) }
}

fn lerp_color(c1: (u8, u8, u8), c2: (u8, u8, u8), t: f64) -> (u8, u8, u8) {
    let t = t.clamp(0.0, 1.0);
    (
        (c1.0 as f64 + (c2.0 as f64 - c1.0 as f64) * t).round() as u8,
        (c1.1 as f64 + (c2.1 as f64 - c1.1 as f64) * t).round() as u8,
        (c1.2 as f64 + (c2.2 as f64 - c1.2 as f64) * t).round() as u8,
    )
}

fn hsv_to_rgb(h: f64, s: f64, v: f64) -> (u8, u8, u8) {
    let h = h.rem_euclid(360.0) / 60.0;
    let c = v * s;
    let x = c * (1.0 - (h % 2.0 - 1.0).abs());
    let m = v - c;
    let (r, g, b) = match h.trunc() as i32 {
        0 => (c, x, 0.0), 1 => (x, c, 0.0), 2 => (0.0, c, x),
        3 => (0.0, x, c), 4 => (x, 0.0, c), _ => (c, 0.0, x),
    };
    (
        ((r + m) * 255.0).round() as u8,
        ((g + m) * 255.0).round() as u8,
        ((b + m) * 255.0).round() as u8,
    )
}

fn ansi_fg_rgb(r: u8, g: u8, b: u8) -> String { format!("\x1b[38;2;{};{};{}m", r, g, b) }
fn ansi_bg_rgb(r: u8, g: u8, b: u8) -> String { format!("\x1b[48;2;{};{};{}m", r, g, b) }
fn ansi_reset() -> &'static str { "\x1b[0m" }
fn color_text_rgb(r: u8, g: u8, b: u8, s: &str) -> String {
    format!("{}{}\x1b[39m", ansi_fg_rgb(r, g, b), s)
}
fn bold_text(s: &str) -> String { format!("\x1b[1m{}\x1b[22m", s) }
fn bold_color_rgb(r: u8, g: u8, b: u8, s: &str) -> String {
    format!("\x1b[1m{}{}\x1b[39m\x1b[22m", ansi_fg_rgb(r, g, b), s)
}
fn visible_len(s: &str) -> usize {
    let mut len = 0;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.next_if_eq(&'[').is_some() {
            while chars.peek().map_or(false, |&d| d != 'm') { chars.next(); }
            chars.next();
        } else { len += 1; }
    }
    len
}

// ── Direct /proc parsers ──────────────────────────────────────────────
fn read_meminfo() -> HashMap<String, u64> {
    let mut map = HashMap::new();
    if let Ok(content) = fs::read_to_string("/proc/meminfo") {
        for line in content.lines() {
            let mut parts = line.splitn(2, ':');
            if let (Some(key), Some(val)) = (parts.next(), parts.next()) {
                if let Some(num) = val.trim().split_whitespace().next().and_then(|s| s.parse().ok()) {
                    map.insert(key.trim().to_string(), num);
                }
            }
        }
    }
    map
}

fn read_cpu_times() -> (u64, u64) {
    if let Ok(content) = fs::read_to_string("/proc/stat") {
        if let Some(line) = content.lines().find(|l| l.starts_with("cpu ")) {
            let fields: Vec<u64> = line.split_whitespace().skip(1)
                .filter_map(|s| s.parse().ok())
                .collect();
            if fields.len() >= 4 {
                let total: u64 = fields.iter().sum();
                let idle = fields[3];
                return (total, idle);
            }
        }
    }
    (0, 0)
}

type UserMap = HashMap<u32, String>;

fn build_user_map() -> UserMap {
    let mut map = HashMap::new();
    if let Ok(content) = fs::read_to_string("/etc/passwd") {
        for line in content.lines() {
            let mut parts = line.splitn(4, ':');
            let name = parts.next();
            parts.next();
            let uid = parts.next().and_then(|s| s.parse::<u32>().ok());
            if let (Some(name), Some(uid)) = (name, uid) {
                map.insert(uid, name.to_string());
            }
        }
    }
    map
}

// ── Process info struct ──────────────────────────────────────────────
#[derive(Clone)]
struct ProcInfo {
    pid: i32,
    user: String,
    cmd: String,
    full_cmd: String,
    pss_kb: u64,
    uss_kb: u64,
    rss_kb: u64,
    swap_kb: u64,
    utime: u64,
    stime: u64,
    state: char,
    cpu_percent: f64,
}

fn read_process_info(pid: i32, user_map: &UserMap) -> Option<ProcInfo> {
    let mut pss_kb = 0u64; let mut rss_kb = 0u64;
    let mut private_clean = 0u64; let mut private_dirty = 0u64; let mut swap_kb = 0u64;
    if let Ok(content) = fs::read_to_string(format!("/proc/{}/smaps_rollup", pid)) {
        for line in content.lines() {
            let mut parts = line.split_whitespace();
            match parts.next() {
                Some("Pss:")           => pss_kb = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0),
                Some("Rss:")           => rss_kb = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0),
                Some("Private_Clean:") => private_clean = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0),
                Some("Private_Dirty:") => private_dirty = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0),
                Some("Swap:")          => swap_kb = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0),
                _ => {}
            }
        }
    }
    let uss_kb = private_clean + private_dirty;

    let uid: u32 = fs::read_to_string(format!("/proc/{}/status", pid))
        .ok()
        .and_then(|s| s.lines()
            .find(|l| l.starts_with("Uid:"))
            .and_then(|l| l.split_whitespace().nth(1)?.parse().ok()))
        .unwrap_or(0);
    let user = user_map.get(&uid).cloned().unwrap_or_else(|| uid.to_string());

    let mut utime = 0u64; let mut stime = 0u64; let mut state = '?';
    if let Ok(content) = fs::read_to_string(format!("/proc/{}/stat", pid)) {
        if let Some(rbracket) = content.rfind(')') {
            let fields: Vec<&str> = content[rbracket+2..].split_whitespace().collect();
            if fields.len() >= 13 {
                state = fields[0].chars().next().unwrap_or('?');
                utime = fields[11].parse().unwrap_or(0);
                stime = fields[12].parse().unwrap_or(0);
            }
        }
    }

    let full_cmd = fs::read_to_string(format!("/proc/{}/cmdline", pid))
        .unwrap_or_default()
        .replace('\0', " ")
        .trim()
        .to_string();
    let full_cmd = full_cmd.chars().take(2040).collect::<String>();
    let cmd = full_cmd.chars().take(40).collect::<String>();

    // FIX #1: Skip processes with 0 RSS (no resident memory = placeholder/zombie)
    if rss_kb == 0 {
        return None;
    }

    Some(ProcInfo { pid, user, cmd, full_cmd, pss_kb, uss_kb, rss_kb, swap_kb, utime, stime, state, cpu_percent: 0.0 })
}

fn get_process_list(user_map: &UserMap) -> Vec<ProcInfo> {
    let mut procs = Vec::new();
    if let Ok(dir) = fs::read_dir("/proc") {
        for entry in dir.flatten() {
            if let Some(pid_str) = entry.file_name().to_str() {
                if pid_str.chars().all(|c| c.is_ascii_digit()) {
                    if let Ok(pid) = pid_str.parse::<i32>() {
                        if let Some(info) = read_process_info(pid, user_map) {
                            procs.push(info);
                        }
                    }
                }
            }
        }
    }
    procs
}

fn sort_procs(procs: &mut Vec<ProcInfo>, col: usize, reverse: bool) {
    match col {
        0 => procs.sort_by_key(|p| p.pid),
        1 => procs.sort_by(|a, b| a.user.to_lowercase().cmp(&b.user.to_lowercase())),
        2 => procs.sort_by(|a, b| a.cmd.to_lowercase().cmp(&b.cmd.to_lowercase())),
        3 => procs.sort_by_key(|p| p.pss_kb),
        4 => procs.sort_by_key(|p| p.uss_kb),
        5 => procs.sort_by_key(|p| p.rss_kb),
        6 => procs.sort_by_key(|p| p.swap_kb),
        7 => procs.sort_by(|a, b| a.cpu_percent.partial_cmp(&b.cpu_percent).unwrap_or(std::cmp::Ordering::Equal)),
        _ => {}
    }
    if reverse { procs.reverse(); }
}

fn color_pss(size_bytes: u64, limit_red_bytes: u64) -> (u8, u8, u8) {
    let green = (0,255,0); let yellow_pale = (255,255,230);
    let yellow_full = (255,255,0); let red = (255,0,0);
    let limit_yellow_pale = 64*1024u64.pow(2); let limit_yellow_full = 512*1024u64.pow(2);
    if size_bytes <= 0 { return green; }
    if size_bytes >= limit_red_bytes { return red; }
    if size_bytes <= limit_yellow_pale { return lerp_color(green, yellow_pale, size_bytes as f64/limit_yellow_pale as f64); }
    if size_bytes <= limit_yellow_full {
        return lerp_color(yellow_pale, yellow_full, (size_bytes-limit_yellow_pale) as f64/(limit_yellow_full-limit_yellow_pale) as f64);
    }
    lerp_color(yellow_full, red, (size_bytes-limit_yellow_full) as f64/(limit_red_bytes-limit_yellow_full) as f64)
}

fn color_rss(size_bytes: u64, mem_total_bytes: u64) -> (u8, u8, u8) {
    let l1=8*1024u64.pow(2); let l2=10*1024u64.pow(2); let l3=128*1024u64.pow(2);
    let l4=136*1024u64.pow(2); let l5=1024*1024u64.pow(2); let l6=1072*1024u64.pow(2);
    let l7=(mem_total_bytes as f64*0.97) as u64; let l8=mem_total_bytes;
    if size_bytes <= l1 { if l1==0 { return (255,0,0); } return hsv_to_rgb((size_bytes as f64/l1 as f64)*360.0,1.0,1.0); }
    else if size_bytes <= l2 { let t=(size_bytes-l1) as f64/(l2-l1) as f64; return hsv_to_rgb(0.0,1.0-0.2*t,1.0); }
    else if size_bytes <= l3 { let t=(size_bytes-l2) as f64/(l3-l2) as f64; return hsv_to_rgb(t*360.0,0.8,1.0); }
    else if size_bytes <= l4 { let t=(size_bytes-l3) as f64/(l4-l3) as f64; return hsv_to_rgb(0.0,0.8-0.3*t,1.0); }
    else if size_bytes <= l5 { let t=(size_bytes-l4) as f64/(l5-l4) as f64; return hsv_to_rgb(t*360.0,0.5,1.0); }
    else if size_bytes <= l6 { let t=(size_bytes-l5) as f64/(l6-l5) as f64; return hsv_to_rgb(0.0,0.5-0.3*t,1.0); }
    else if size_bytes <= l7 { let t=(size_bytes-l6) as f64/(l7-l6) as f64; return hsv_to_rgb(t*360.0,0.2,1.0); }
    else if size_bytes <= l8 { return lerp_color((255,204,204),(128,128,128),(size_bytes-l7) as f64/(l8-l7) as f64); }
    (128,128,128)
}

fn color_swap(size_bytes: u64, swap_total_bytes: u64) -> (u8, u8, u8) {
    let red=(255,0,0); let pink=(255,128,128); let limit_pink=1*1024u64.pow(3);
    if size_bytes >= swap_total_bytes { return red; }
    if size_bytes >= limit_pink { return lerp_color(pink,red,(size_bytes-limit_pink) as f64/(swap_total_bytes-limit_pink) as f64); }
    if size_bytes > 0 { return lerp_color(WHITE_PURE,pink,size_bytes as f64/limit_pink as f64); }
    WHITE_PURE
}

fn color_cpu(percent: f64) -> (u8, u8, u8) {
    let p = percent.clamp(0.0,100.0)/100.0;
    if p < 0.5 { lerp_color((0,255,0),(255,255,0), p*2.0) }
    else { lerp_color((255,255,0),(255,0,0), (p-0.5)*2.0) }
}

struct App {
    term_width: usize, term_height: usize, interval: f64, no_color: bool,
    current_user: String, clk_tck: u64, num_cpus: u64,
    mem_total_bytes: u64, limit_red_bytes: u64, swap_total_bytes: u64,
    user_map: UserMap,
    raw_data: Vec<ProcInfo>, filtered_raw: Vec<ProcInfo>, data_rows: Vec<Vec<String>>,
    widths: Vec<usize>, sort_col: usize, sort_reverse: bool,
    filter_string: String, searching: bool, killing: bool, kill_pid_str: String, cmd_offset: usize,
    previous_cpu_times: HashMap<i32, (u64, Instant)>,
    prev_sys_cpu: Option<(u64, u64)>,
    system_info: SystemInfo,
    scroll_pos: usize, last_refresh: Instant,
}

#[derive(Default)]
struct SystemInfo {
    cpu_percent: f64, mem_percent: f64, swap_percent: f64,
    load: String, uptime: String, tasks: String,
}

impl App {
    fn new(interval: f64, no_color: bool) -> Self {
        let (w, h) = terminal::size().unwrap_or((80,24));
        let meminfo = read_meminfo();
        let mem_total_bytes = *meminfo.get("MemTotal").unwrap_or(&(16*1024*1024)) * 1024;
        let swap_total_bytes = *meminfo.get("SwapTotal").unwrap_or(&(32*1024*1024)) * 1024;
        let limit_red_bytes = (mem_total_bytes as f64 * 0.99) as u64;
        let clk_tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as u64;
        let num_cpus = num_cpus::get() as u64;
        let current_user = env::var("SUDO_USER").ok()
            .or_else(|| env::var("USER").ok())
            .unwrap_or_default();
        Self {
            term_width: w as usize, term_height: h as usize,
            interval, no_color, current_user, clk_tck, num_cpus,
            mem_total_bytes, limit_red_bytes, swap_total_bytes,
            user_map: build_user_map(),
            raw_data: vec![], filtered_raw: vec![], data_rows: vec![],
            widths: vec![0;8], sort_col: 3, sort_reverse: true,
            filter_string: String::new(), searching: false, killing: false,
            kill_pid_str: String::new(), cmd_offset: 0,
            previous_cpu_times: HashMap::new(), prev_sys_cpu: None,
            system_info: SystemInfo::default(), scroll_pos: 0,
            last_refresh: Instant::now(),
        }
    }

    fn full_update(&mut self) {
        self.raw_data = get_process_list(&self.user_map);
        let now = Instant::now();
        for proc in &mut self.raw_data {
            let cur = proc.utime + proc.stime;
            if let Some(&(prev, prev_time)) = self.previous_cpu_times.get(&proc.pid) {
                let delta = cur.saturating_sub(prev);
                let dt = now.duration_since(prev_time).as_secs_f64();
                if dt > 0.0 {
                    proc.cpu_percent = (delta as f64 / self.clk_tck as f64 / self.num_cpus as f64 / dt) * 100.0;
                }
            }
            self.previous_cpu_times.insert(proc.pid, (cur, now));
        }
        self.previous_cpu_times.retain(|&pid,_| self.raw_data.iter().any(|p| p.pid==pid));

        let (total, idle) = read_cpu_times();
        let cpu_percent = if let Some((prev_total, prev_idle)) = self.prev_sys_cpu {
            let d_total = total.saturating_sub(prev_total);
            let d_idle = idle.saturating_sub(prev_idle);
            if d_total > 0 { (1.0 - d_idle as f64 / d_total as f64) * 100.0 } else { 0.0 }
        } else { 0.0 };
        self.prev_sys_cpu = Some((total, idle));

        let meminfo = read_meminfo();
        let mem_total = *meminfo.get("MemTotal").unwrap_or(&0);
        let mem_avail = *meminfo.get("MemAvailable").unwrap_or(&{
            meminfo.get("MemFree").unwrap_or(&0)
                + meminfo.get("Buffers").unwrap_or(&0)
                + meminfo.get("Cached").unwrap_or(&0)
        });
        let mem_used = mem_total.saturating_sub(mem_avail);
        let mem_percent = if mem_total > 0 { (mem_used as f64 / mem_total as f64) * 100.0 } else { 0.0 };

        let swap_total = *meminfo.get("SwapTotal").unwrap_or(&0);
        let swap_free = *meminfo.get("SwapFree").unwrap_or(&0);
        let swap_used = swap_total.saturating_sub(swap_free);
        let swap_percent = if swap_total > 0 { (swap_used as f64 / swap_total as f64) * 100.0 } else { 0.0 };

        let load = fs::read_to_string("/proc/loadavg").ok()
            .map(|s| s.split_whitespace().take(3).collect::<Vec<_>>().join(" "))
            .unwrap_or_default();
        let uptime = fs::read_to_string("/proc/uptime").ok()
            .and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok())
            .map(|s| {
                let s = s as u64;
                let y = s/(365*86400); let r = s%(365*86400);
                let d = r/86400; let r = r%86400; let h = r/3600; let r = r%3600;
                let m = r/60; let sec = r%60;
                format!("{}y {}d {}h {}m {}s", y, d, h, m, sec)
            }).unwrap_or_default();
        let (mut run, mut slp, mut zom) = (0,0,0);
        for p in &self.raw_data {
            match p.state {
                'R' => run+=1, 'S'|'D' => slp+=1, 'Z' => zom+=1, _ => {}
            }
        }
        let tasks = format!("{} ({} run, {} slp, {} zom)", self.raw_data.len(), run, slp, zom);
        self.system_info = SystemInfo { cpu_percent, mem_percent, swap_percent, load, uptime, tasks };
        self.apply_filter_and_sort();
    }

    fn apply_filter_and_sort(&mut self) {
        if self.filter_string.is_empty() {
            self.filtered_raw = self.raw_data.clone();
        } else {
            let lower = self.filter_string.to_lowercase();
            self.filtered_raw = self.raw_data.iter()
                .filter(|p| p.full_cmd.to_lowercase().contains(&lower))
                .cloned().collect();
        }
        sort_procs(&mut self.filtered_raw, self.sort_col, self.sort_reverse);
        self.data_rows = self.filtered_raw.iter().map(|p| self.format_row(p)).collect();
    }

    fn soft_update(&mut self) {
        sort_procs(&mut self.filtered_raw, self.sort_col, self.sort_reverse);
        self.data_rows = self.filtered_raw.iter().map(|p| self.format_row(p)).collect();
    }

    // FIX #2: UTF-8 safe command scrolling
    fn format_row(&self, p: &ProcInfo) -> Vec<String> {
        let chars: Vec<char> = p.full_cmd.chars().collect();
        let max_offset = chars.len().saturating_sub(40);
        let offset = self.cmd_offset.min(max_offset);
        let displayed: String = chars.iter().skip(offset).take(40).collect();
        let cmd_str = format!("{: <40}", displayed);
        
        let pid = format!("{}", p.pid);
        let user = &p.user;

        let pid_cell = if self.no_color { pid.clone() } else { color_text_rgb(YELLOW_BRIGHT.0, YELLOW_BRIGHT.1, YELLOW_BRIGHT.2, &pid) };
        let user_cell = if self.no_color { user.clone() } else {
            let (r,g,b) = if *user == self.current_user { BLUE_BRIGHT } else if user == "root" { RED_DESAT } else { INDIGO_DESAT };
            color_text_rgb(r,g,b, user)
        };
        let cmd_cell = if self.no_color { cmd_str.clone() } else { color_text_rgb(WHITE_DIMMER.0, WHITE_DIMMER.1, WHITE_DIMMER.2, &cmd_str) };

        let pss = format_memory(p.pss_kb); let uss = format_memory(p.uss_kb);
        let rss = format_memory(p.rss_kb); let swap = format_memory(p.swap_kb);
        let cpu = format!("{:5.1}", p.cpu_percent);

        let pss_cell = if self.no_color { pss.clone() } else { let (r,g,b)=color_pss(p.pss_kb*1024,self.limit_red_bytes); color_text_rgb(r,g,b,&pss) };
        let uss_cell = if self.no_color { uss.clone() } else { let (r,g,b)=color_pss(p.uss_kb*1024,self.limit_red_bytes); color_text_rgb(r,g,b,&uss) };
        let rss_cell = if self.no_color { rss.clone() } else { let (r,g,b)=color_rss(p.rss_kb*1024,self.mem_total_bytes); color_text_rgb(r,g,b,&rss) };
        let swap_cell = if self.no_color { swap.clone() } else { let (r,g,b)=color_swap(p.swap_kb*1024,self.swap_total_bytes); color_text_rgb(r,g,b,&swap) };
        let cpu_cell = if self.no_color { cpu.clone() } else { let (r,g,b)=color_cpu(p.cpu_percent); color_text_rgb(r,g,b,&cpu) };

        vec![pid_cell, user_cell, cmd_cell, pss_cell, uss_cell, rss_cell, swap_cell, cpu_cell]
    }

    fn compute_widths(&mut self) {
        let headers = ["1 PID","2 User","3 Command","4 PSS","5 USS","6 RSS","7 Swap","8 CPU%"];
        self.widths = headers.iter().map(|h| visible_len(h)).collect::<Vec<_>>();
        for row in &self.data_rows {
            for (i, cell) in row.iter().enumerate() {
                let v = visible_len(cell);
                if v > self.widths[i] { self.widths[i] = v; }
            }
        }
    }

    fn build_header_line(&self) -> String {
        let headers = ["1 PID","2 User","3 Command","4 PSS","5 USS","6 RSS","7 Swap","8 CPU%"];
        let positions: [usize; 8] = [1,8,24,69,79,89,98,105];
        let mut line = String::new(); let mut col = 0;
        for (i, label) in headers.iter().enumerate() {
            let pos = positions[i];
            let spaces = pos.saturating_sub(col);
            line.push_str(&" ".repeat(spaces));
            let fmt = if i == self.sort_col && !self.no_color {
                bold_color_rgb(CYAN_BRIGHT.0, CYAN_BRIGHT.1, CYAN_BRIGHT.2, label)
            } else if !self.no_color {
                bold_color_rgb(255,255,255, label)
            } else { bold_text(label) };
            line.push_str(&fmt);
            col = pos + label.len();
        }
        let max = self.term_width.saturating_sub(2);
        if col < max { line.push_str(&" ".repeat(max - col)); }
        line.push_str(&format!("{}\x1b[0m  ", ansi_bg_rgb(40,40,40)));
        line
    }

    fn pad_cell(cell: &str, width: usize, align: &str) -> String {
        let v = visible_len(cell);
        if v >= width { return cell.to_string(); }
        let pad = " ".repeat(width - v);
        if align == "right" { format!("{}{}", pad, cell) } else { format!("{}{}", cell, pad) }
    }

    fn build_system_line(&self) -> String {
        let si = &self.system_info;
        let bar = |pct: f64, w: usize| -> String {
            let filled = ((pct/100.0) * w as f64).round() as usize;
            let filled = filled.min(w);
            if self.no_color { format!("{}{}", "|".repeat(filled), " ".repeat(w-filled)) }
            else {
                let (r,g,b) = color_cpu(pct);
                format!("{}{}{}{}", ansi_fg_rgb(r,g,b), "|".repeat(filled), "\x1b[39m", " ".repeat(w-filled))
            }
        };
        let cpu = format!("{} {} {:5.1}%", bold_text("CPU"), bar(si.cpu_percent,8), si.cpu_percent);
        let mem = format!("{} {} {:5.1}%", bold_text("Mem"), bar(si.mem_percent,8), si.mem_percent);
        let swp = format!("{} {} {:5.1}%", bold_text("Swp"), bar(si.swap_percent,4), si.swap_percent);
        let mut line = format!("{} {} {}", cpu, mem, swp);
        let try_append = |line: &mut String, prefix: &str, val: &str| {
            if val.is_empty() { return; }
            let candidate = format!(" {} {} {}", line, bold_text(prefix), val);
            if visible_len(&candidate) <= self.term_width { *line = candidate; }
        };
        try_append(&mut line, "Load 1/5/15min", &si.load);
        try_append(&mut line, "Uptime", &si.uptime);
        try_append(&mut line, "Tasks:", &si.tasks);
        let vis = visible_len(&line);
        if vis < self.term_width { line.push_str(&" ".repeat(self.term_width - vis)); }
        else { while visible_len(&line) > self.term_width { line.pop(); } line.push_str(&" ".repeat(self.term_width - visible_len(&line))); }
        line
    }

    fn render(&mut self) -> io::Result<()> {
        let total = self.data_rows.len();
        let visible = self.term_height.saturating_sub(3);
        let max_scroll = if total > visible { total - visible } else { 0 };
        self.scroll_pos = self.scroll_pos.min(max_scroll);

        let sys = self.build_system_line();
        let header = self.build_header_line();
        let gaps = [2,1,2,2,2,2,2];
        let aligns = ["right","left","left","right","right","right","right","right"];
        let track = ansi_bg_rgb(40,40,40); let thumb = ansi_bg_rgb(120,120,120); let rst = ansi_reset();

        let mut lines = Vec::with_capacity(self.term_height);
        lines.push(sys); lines.push(header);
        for idx in 0..visible {
            let didx = self.scroll_pos + idx;
            let row = if didx < total {
                let cells: Vec<String> = self.data_rows[didx].iter().enumerate()
                    .map(|(i,c)| Self::pad_cell(c, self.widths[i], aligns[i])).collect();
                let mut line = String::new();
                for (i,c) in cells.iter().enumerate() { line.push_str(c); if i<gaps.len() { line.push_str(&" ".repeat(gaps[i])); } }
                let maxw = self.term_width.saturating_sub(2);
                let v = visible_len(&line); if v < maxw { line.push_str(&" ".repeat(maxw-v)); }
                line
            } else { " ".repeat(self.term_width.saturating_sub(2)) };
            let scroll = if total > visible && total > 0 {
                let thumb_h = std::cmp::max(1, (visible as f64 * visible as f64 / total as f64).round() as usize);
                let thumb_top = if total > visible {
                    ((visible - thumb_h) as f64 * (self.scroll_pos as f64 / (total - visible) as f64)).round() as usize
                } else { 0 };
                if idx >= thumb_top && idx < thumb_top+thumb_h { format!("{thumb}  {rst}") } else { format!("{track}  {rst}") }
            } else { "  ".to_string() };
            lines.push(format!("{}{}", row, scroll));
        }

        let totals = self.filtered_raw.iter().fold((0,0), |(pss,swp), p| (pss+p.pss_kb, swp+p.swap_kb));
        let tot_text = format!("PSS: {}  Swap: {}", format_memory(totals.0), format_memory(totals.1));
        let int_str = if self.interval.fract()==0.0 { format!("{}s", self.interval as u64) } else { format!("{}s", self.interval) };
        let headers = ["1 PID","2 User","3 Command","4 PSS","5 USS","6 RSS","7 Swap","8 CPU%"];
        let arrow = if self.sort_reverse { "↓" } else { "↑" };
        let sort = format!("{} {}", headers[self.sort_col], arrow);
        let left = if self.killing { format!("Kill PID: {}_ (Enter to confirm, Esc to cancel)", self.kill_pid_str) }
        else if self.searching { format!("Search: {}_ (Enter to accept, Esc to clear)", self.filter_string) }
        else if !self.filter_string.is_empty() { format!("Filter: '{}' | {} | Interval: {} | / search", self.filter_string, sort, int_str) }
        else { format!("Interval: {} | +/- change | q quit | k kill | ←→ cmd | ↑↓ scroll (5 lines) | Sort: {}", int_str, sort) };
        let left = if self.cmd_offset > 0 { format!("{}  Cmd offset: {}", left, self.cmd_offset) } else { left };
        let status = if left.len() + tot_text.len() + 1 <= self.term_width {
            format!("{}{}{}", left, " ".repeat(self.term_width - left.len() - tot_text.len() - 1), tot_text)
        } else {
            let trunc: String = left.chars().take(self.term_width.saturating_sub(tot_text.len()+1)).collect();
            format!("{} {}", trunc, tot_text)
        };
        let status_line = if self.no_color { format!("{: <width$}", status, width=self.term_width) }
        else { format!("{}{}{}{}", ansi_bg_rgb(60,60,60), ansi_fg_rgb(255,255,255), format!("{: <width$}", status, width=self.term_width), rst) };
        lines.push(status_line);

        let mut out = io::stdout();
        execute!(out, cursor::MoveTo(0,0))?;
        for (i,line) in lines.iter().enumerate() {
            write!(out, "{}", line)?;
            if i < lines.len() - 1 {
                write!(out, "\r\n")?;
            }
        }
        execute!(out, terminal::Clear(ClearType::FromCursorDown))?;
        out.flush()?;
        Ok(())
    }
}

fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().collect();
    let mut interval = 2.0_f64;
    let mut no_color = false;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-i"|"--interval" => { i+=1; if i<args.len() { interval = args[i].parse().unwrap_or(2.0); } }
            "--no-color" => no_color = true,
            _ => {}
        }
        i+=1;
    }
    let intervals: [f64;16] = [0.05,0.1,0.25,0.5,1.0,2.0,3.0,5.0,10.0,15.0,20.0,30.0,60.0,120.0,180.0,300.0];
    if !intervals.contains(&interval) {
        interval = intervals.iter().min_by(|a,b| (**a-interval).abs().partial_cmp(&(**b-interval).abs()).unwrap()).copied().unwrap_or(2.0);
    }

    // Removed unused `let mut out = io::stdout();` to fix compiler warnings
    let _guard = TerminalGuard::new()?;
    
    let mut app = App::new(interval, no_color);
    app.full_update(); app.compute_widths(); app.scroll_pos=0;
    app.render()?;
    app.last_refresh = Instant::now();

    'outer: loop {
        let timeout = if app.searching || app.killing { Duration::from_secs_f64(0.1) } else { Duration::from_secs_f64(app.interval) };
        if event::poll(timeout)? {
            let mut keys = Vec::new();
            loop {
                if let Event::Key(k) = event::read()? { keys.push(k); }
                if !event::poll(Duration::ZERO)? { break; }
            }
            for key in keys {
                if app.killing {
                    match key.code {
                        KeyCode::Esc => { app.killing=false; app.kill_pid_str.clear(); app.render()?; continue; }
                        KeyCode::Enter => {
                            if !app.kill_pid_str.is_empty() && app.kill_pid_str.chars().all(|c| c.is_ascii_digit()) {
                                if let Ok(pid) = app.kill_pid_str.parse::<i32>() { unsafe { libc::kill(pid, libc::SIGTERM); } }
                            }
                            app.killing=false; app.kill_pid_str.clear(); app.full_update(); app.scroll_pos=0;
                            app.last_refresh=Instant::now(); app.render()?; continue;
                        }
                        KeyCode::Backspace|KeyCode::Delete => { app.kill_pid_str.pop(); app.render()?; continue; }
                        KeyCode::Char(c) if c.is_ascii_digit() => { app.kill_pid_str.push(c); app.render()?; continue; }
                        _ => continue,
                    }
                }
                if app.searching {
                    match key.code {
                        KeyCode::Esc => { app.filter_string.clear(); app.searching=false; app.full_update(); app.scroll_pos=0; app.last_refresh=Instant::now(); app.render()?; continue; }
                        KeyCode::Enter => { app.searching=false; app.full_update(); app.scroll_pos=0; app.last_refresh=Instant::now(); app.render()?; continue; }
                        KeyCode::Backspace|KeyCode::Delete => { app.filter_string.pop(); app.render()?; continue; }
                        KeyCode::Char(c) if c.is_ascii_graphic()||c==' ' => { app.filter_string.push(c); app.render()?; continue; }
                        _ => continue,
                    }
                }
                match key.code {
                    KeyCode::Char('q')|KeyCode::Char('Q') => break 'outer,
                    KeyCode::Char('/') => { app.searching=true; app.render()?; continue; }
                    KeyCode::Char('k')|KeyCode::Char('K') => { app.killing=true; app.kill_pid_str.clear(); app.render()?; continue; }
                    KeyCode::Esc => { app.filter_string.clear(); app.full_update(); app.scroll_pos=0; app.last_refresh=Instant::now(); app.render()?; continue; }
                    KeyCode::Up => { app.scroll_pos = app.scroll_pos.saturating_sub(5); app.render()?; }
                    KeyCode::Down => {
                        let vis = app.term_height.saturating_sub(3);
                        let max = if app.data_rows.len()>vis { app.data_rows.len()-vis } else { 0 };
                        app.scroll_pos = (app.scroll_pos+5).min(max); app.render()?;
                    }
                    KeyCode::Left => { app.cmd_offset = app.cmd_offset.saturating_sub(40); app.soft_update(); app.render()?; continue; }
                    KeyCode::Right => { app.cmd_offset = (app.cmd_offset+40).min(2000); app.soft_update(); app.render()?; continue; }
                    KeyCode::Char('+')|KeyCode::Char('=') => {
                        if let Some(pos) = intervals.iter().position(|&v| v==app.interval) {
                            if pos < intervals.len()-1 { app.interval = intervals[pos+1]; app.last_refresh=Instant::now(); app.full_update(); app.compute_widths(); app.render()?; }
                        }
                    }
                    KeyCode::Char('-') => {
                        if let Some(pos) = intervals.iter().position(|&v| v==app.interval) {
                            if pos > 0 { app.interval = intervals[pos-1]; app.last_refresh=Instant::now(); app.full_update(); app.compute_widths(); app.render()?; }
                        }
                    }
                    KeyCode::Char(c) if c.is_ascii_digit() => {
                        let n = c.to_digit(10).unwrap() as usize;
                        if (1..=8).contains(&n) {
                            let col = n-1;
                            if col == app.sort_col { app.sort_reverse = !app.sort_reverse; }
                            else { app.sort_col = col; app.sort_reverse = col>2; }
                            app.soft_update(); app.scroll_pos=0; app.render()?; app.last_refresh=Instant::now();
                        }
                    }
                    _ => {}
                }
            }
        }
        if !app.killing && app.last_refresh.elapsed().as_secs_f64() >= app.interval {
            app.full_update(); app.compute_widths(); app.last_refresh = Instant::now();
            let vis = app.term_height.saturating_sub(3);
            let max = if app.data_rows.len()>vis { app.data_rows.len()-vis } else { 0 };
            app.scroll_pos = app.scroll_pos.min(max);
            if app.searching { app.scroll_pos = 0; }
            app.render()?;
        }
        if terminal::size().map(|(w,h)| w as usize != app.term_width || h as usize != app.term_height).unwrap_or(false) {
            let (w,h) = terminal::size().unwrap_or((80,24));
            app.term_width = w as usize; app.term_height = h as usize;
            app.compute_widths(); app.render()?;
        }
    }
    Ok(())
}
