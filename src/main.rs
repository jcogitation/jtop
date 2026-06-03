use crossterm::{
    cursor, execute,
    terminal::{self, ClearType},
    event::{self, Event, KeyCode},
};
use std::{
    collections::{HashMap, HashSet},
    env,
    fs,
    io::{self, Write},
    time::{Duration, Instant},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc,
    },
    thread,
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
    state: char,
    cpu_percent: f64,
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
    current_user: String,
    mem_total_bytes: u64, limit_red_bytes: u64, swap_total_bytes: u64,
    raw_data: Vec<ProcInfo>, filtered_raw: Vec<ProcInfo>, data_rows: Vec<Vec<String>>,
    widths: Vec<usize>, sort_col: usize, sort_reverse: bool,
    filter_string: String, searching: bool, 
    kill_mode: bool, kill_selected: usize, marked_pids: HashSet<i32>, kill_confirming: bool,
    cmd_offset: usize,
    system_info: SystemInfo,
    scroll_pos: usize, mem_override: bool,
}

#[derive(Default, Clone)]
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
        let current_user = env::var("SUDO_USER").ok()
            .or_else(|| env::var("USER").ok())
            .unwrap_or_default();
        Self {
            term_width: w as usize, term_height: h as usize,
            interval, no_color, current_user,
            mem_total_bytes, limit_red_bytes, swap_total_bytes,
            raw_data: vec![], filtered_raw: vec![], data_rows: vec![],
            widths: vec![0;8], sort_col: 3, sort_reverse: true,
            filter_string: String::new(), searching: false,
            kill_mode: false, kill_selected: 0, marked_pids: HashSet::new(), kill_confirming: false,
            cmd_offset: 0,
            system_info: SystemInfo::default(), scroll_pos: 0,
            mem_override: false,
        }
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

    fn build_header_line(&self, kill_mode: bool) -> String {
        let headers = ["1 PID","2 User","3 Command","4 PSS","5 USS","6 RSS","7 Swap","8 CPU%"];
        let mut positions: [usize; 8] = [1,8,24,69,79,89,98,105];
        if kill_mode {
            for p in &mut positions { *p += 4; }
        }
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
        let header = self.build_header_line(self.kill_mode);
        let gaps = [2,1,2,2,2,2,2];
        let aligns = ["right","left","left","right","right","right","right","right"];
        let track = ansi_bg_rgb(40,40,40); let thumb = ansi_bg_rgb(120,120,120); let rst = ansi_reset();

        let mut out = io::stdout().lock();
        execute!(out, cursor::MoveTo(0,0))?;
        
        write!(out, "{}\r\n", sys)?;
        write!(out, "{}\r\n", header)?;
        
        for idx in 0..visible {
            let didx = self.scroll_pos + idx;
            let row = if didx < total {
                let cells: Vec<String> = self.data_rows[didx].iter().enumerate()
                    .map(|(i,c)| Self::pad_cell(c, self.widths[i], aligns[i])).collect();
                let mut line = String::new();
                for (i,c) in cells.iter().enumerate() { line.push_str(c); if i<gaps.len() { line.push_str(&" ".repeat(gaps[i])); } }
                
                if self.kill_mode && didx < self.filtered_raw.len() {
                    let pid = self.filtered_raw[didx].pid;
                    let is_marked = self.marked_pids.contains(&pid);
                    let is_selected = didx == self.kill_selected;
                    let marker = if is_selected && is_marked { "[*] " }
                                 else if is_marked { "[x] " }
                                 else if is_selected { "[ ] " }
                                 else { "    " };
                    
                    if is_selected {
                        line = format!("\x1b[7m{}{}\x1b[27m", marker, line);
                    } else {
                        line = format!("{}{}", marker, line);
                    }
                }

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
            write!(out, "{}{}\r\n", row, scroll)?;
        }

        let totals = self.filtered_raw.iter().fold((0,0), |(pss,swp), p| (pss+p.pss_kb, swp+p.swap_kb));
        let tot_text = format!("PSS: {}  Swap: {}", format_memory(totals.0), format_memory(totals.1));
        let int_str = if self.interval.fract()==0.0 { format!("{}s", self.interval as u64) } else { format!("{}s", self.interval) };
        let headers = ["1 PID","2 User","3 Command","4 PSS","5 USS","6 RSS","7 Swap","8 CPU%"];
        let arrow = if self.sort_reverse { "↓" } else { "↑" };
        let sort = format!("{} {}", headers[self.sort_col], arrow);
        
        let cpu_mode = if self.interval < 0.5 { " [NANO CPU]" } else { "" };
        
        let left = if self.kill_confirming {
            format!("Kill {} marked process(es)? Are you sure? y/n", self.marked_pids.len())
        } else if self.kill_mode {
            let marked_count = self.marked_pids.len();
            format!("KILL MODE: ↑↓ select | Space mark | Enter confirm | Esc cancel | Marked: {}", marked_count)
        } else if self.searching { 
            format!("Search: {}_ (Enter to accept, Esc to clear)", self.filter_string) 
        } else if !self.filter_string.is_empty() { 
            format!("Filter: '{}' | {} | Interval: {}{} | / search", self.filter_string, sort, int_str, cpu_mode) 
        } else { 
            format!("Interval: {}{} | +/- change | m mem-override | q quit | k kill | ←→ cmd | ↑↓ scroll | Sort: {}", int_str, cpu_mode, sort) 
        };
        
        let left = if !self.kill_mode && !self.kill_confirming && self.cmd_offset > 0 { 
            format!("{}  Cmd offset: {}", left, self.cmd_offset) 
        } else { left };
        let left = if !self.kill_mode && !self.kill_confirming && self.mem_override { 
            format!("{} [MEM OVERRIDE]", left) 
        } else { left };
        
        let status = if left.len() + tot_text.len() + 1 <= self.term_width {
            format!("{}{}{}", left, " ".repeat(self.term_width - left.len() - tot_text.len() - 1), tot_text)
        } else {
            let trunc: String = left.chars().take(self.term_width.saturating_sub(tot_text.len()+1)).collect();
            format!("{} {}", trunc, tot_text)
        };
        let status_bg = if self.kill_confirming { ansi_bg_rgb(180, 40, 40) } 
                        else if self.kill_mode { ansi_bg_rgb(40, 60, 100) } 
                        else if self.searching { ansi_bg_rgb(80, 100, 40) }
                        else { ansi_bg_rgb(60,60,60) };
        let status_line = if self.no_color { format!("{: <width$}", status, width=self.term_width) }
        else { format!("{}{}{}{}", status_bg, ansi_fg_rgb(255,255,255), format!("{: <width$}", status, width=self.term_width), rst) };
        
        write!(out, "{}", status_line)?;
        execute!(out, terminal::Clear(ClearType::FromCursorDown))?;
        out.flush()?;
        Ok(())
    }
}

// ── Background Worker Logic ─────────────────────────────────────────
struct WorkerState {
    prev_cpu_times: HashMap<i32, (f64, Instant)>, 
    prev_sys_cpu: Option<(u64, u64)>,
    cached_static: HashMap<i32, (String, String, String)>, 
    last_procs: HashMap<i32, ProcInfo>,
    last_mem_update: Instant,
    last_use_nano_mode: bool,
}

fn gather_data(
    do_mem: bool,
    use_nano_mode: bool,
    state: &mut WorkerState,
    user_map: &UserMap,
    clk_tck: u64,
    num_cpus: u64,
) -> (Vec<ProcInfo>, SystemInfo) {
    let mut procs = Vec::new();
    let now = Instant::now();

    if let Ok(dir) = fs::read_dir("/proc") {
        for entry in dir.flatten() {
            if let Some(pid_str) = entry.file_name().to_str() {
                if pid_str.chars().all(|c| c.is_ascii_digit()) {
                    if let Ok(pid) = pid_str.parse::<i32>() {
                        let stat_path = format!("/proc/{}/stat", pid);
                        let stat_content = match fs::read_to_string(&stat_path) {
                            Ok(c) => c,
                            Err(_) => continue,
                        };
                        
                        let (state_char, utime, stime) = if let Some(rbracket) = stat_content.rfind(')') {
                            let fields: Vec<&str> = stat_content[rbracket+2..].split_whitespace().collect();
                            if fields.len() >= 13 {
                                (fields[0].chars().next().unwrap_or('?'), 
                                 fields[11].parse::<u64>().unwrap_or(0), 
                                 fields[12].parse::<u64>().unwrap_or(0))
                            } else { continue; }
                        } else { continue; };

                        let mut use_nano_for_this_proc = use_nano_mode;
                        let mut runtime_ns = 0.0;
                        
                        if use_nano_for_this_proc {
                            let task_dir = format!("/proc/{}/task", pid);
                            let mut sum_ns = 0.0;
                            let mut found_any = false;
                            
                            if let Ok(tasks) = fs::read_dir(&task_dir) {
                                for task in tasks.flatten() {
                                    let schedstat_path = format!("{}/{}/schedstat", task_dir, task.file_name().to_string_lossy());
                                    if let Ok(content) = fs::read_to_string(&schedstat_path) {
                                        if let Some(ns_str) = content.split_whitespace().next() {
                                            if let Ok(ns) = ns_str.parse::<f64>() {
                                                sum_ns += ns;
                                                found_any = true;
                                            }
                                        }
                                    }
                                }
                            }
                            
                            if found_any {
                                runtime_ns = sum_ns;
                            } else {
                                use_nano_for_this_proc = false;
                            }
                        }

                        let (mut pss_kb, mut uss_kb, mut rss_kb, mut swap_kb) = (0, 0, 0, 0);
                        if do_mem {
                            let smaps_path = format!("/proc/{}/smaps_rollup", pid);
                            if let Ok(content) = fs::read_to_string(&smaps_path) {
                                let mut private_clean = 0u64; let mut private_dirty = 0u64;
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
                                uss_kb = private_clean + private_dirty;
                            }
                        } else if let Some(prev) = state.last_procs.get(&pid) {
                            pss_kb = prev.pss_kb;
                            uss_kb = prev.uss_kb;
                            rss_kb = prev.rss_kb;
                            swap_kb = prev.swap_kb;
                        }

                        let (user, cmd, full_cmd) = if let Some(cached) = state.cached_static.get(&pid) {
                            cached.clone()
                        } else {
                            let uid: u32 = fs::read_to_string(format!("/proc/{}/status", pid))
                                .ok()
                                .and_then(|s| s.lines().find(|l| l.starts_with("Uid:")).and_then(|l| l.split_whitespace().nth(1)?.parse().ok()))
                                .unwrap_or(0);
                            let user = user_map.get(&uid).cloned().unwrap_or_else(|| uid.to_string());
                            let full_cmd = fs::read_to_string(format!("/proc/{}/cmdline", pid))
                                .unwrap_or_default().replace('\0', " ").trim().to_string();
                            let full_cmd = full_cmd.chars().take(2040).collect::<String>();
                            let cmd = full_cmd.chars().take(40).collect::<String>();
                            let res = (user, cmd, full_cmd);
                            state.cached_static.insert(pid, res.clone());
                            res
                        };

                        if rss_kb == 0 { continue; }

                        let current_val = if use_nano_for_this_proc { runtime_ns } else { (utime + stime) as f64 };
                        let mut cpu_percent = 0.0;
                        
                        if let Some(&(prev_val, prev_time)) = state.prev_cpu_times.get(&pid) {
                            let delta = current_val - prev_val;
                            let dt = now.duration_since(prev_time).as_secs_f64();
                            if dt > 0.0 && delta >= 0.0 {
                                if use_nano_for_this_proc {
                                    cpu_percent = (delta / 1_000_000_000.0 / dt / num_cpus as f64) * 100.0;
                                } else {
                                    cpu_percent = (delta / clk_tck as f64 / dt / num_cpus as f64) * 100.0;
                                }
                            }
                        }
                        state.prev_cpu_times.insert(pid, (current_val, now));

                        procs.push(ProcInfo {
                            pid, user, cmd, full_cmd, pss_kb, uss_kb, rss_kb, swap_kb,
                            state: state_char, cpu_percent
                        });
                    }
                }
            }
        }
    }
    
    let active_pids: HashSet<i32> = procs.iter().map(|p| p.pid).collect();
    state.prev_cpu_times.retain(|pid, _| active_pids.contains(pid));
    state.cached_static.retain(|pid, _| active_pids.contains(pid));

    let (total, idle) = read_cpu_times();
    let cpu_percent = if let Some((prev_total, prev_idle)) = state.prev_sys_cpu {
        let d_total = total.saturating_sub(prev_total);
        let d_idle = idle.saturating_sub(prev_idle);
        if d_total > 0 { (1.0 - d_idle as f64 / d_total as f64) * 100.0 } else { 0.0 }
    } else { 0.0 };
    state.prev_sys_cpu = Some((total, idle));

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
    for p in &procs {
        match p.state {
            'R' => run+=1, 'S'|'D' => slp+=1, 'Z' => zom+=1, _ => {}
        }
    }
    let tasks = format!("{} ({} run, {} slp, {} zom)", procs.len(), run, slp, zom);
    
    let sys_info = SystemInfo { cpu_percent, mem_percent, swap_percent, load, uptime, tasks };

    (procs, sys_info)
}

fn worker_loop(
    tx: mpsc::Sender<(Vec<ProcInfo>, SystemInfo)>,
    interval_shared: Arc<AtomicU64>,
    mem_override_shared: Arc<AtomicBool>,
    user_map: UserMap,
    clk_tck: u64,
    num_cpus: u64,
) {
    let mut state = WorkerState {
        prev_cpu_times: HashMap::new(),
        prev_sys_cpu: None,
        cached_static: HashMap::new(),
        last_procs: HashMap::new(),
        last_mem_update: Instant::now() - Duration::from_secs(10),
        last_use_nano_mode: false,
    };

    let priming_int = f64::from_bits(interval_shared.load(Ordering::Relaxed));
    let priming_nano = priming_int < 0.5;
    let _ = gather_data(true, priming_nano, &mut state, &user_map, clk_tck, num_cpus);
    thread::sleep(Duration::from_millis(100));

    loop {
        let start = Instant::now();
        let int_sec = f64::from_bits(interval_shared.load(Ordering::Relaxed));
        
        let use_nano_mode = int_sec < 0.5; 
        
        if state.last_use_nano_mode != use_nano_mode {
            state.prev_cpu_times.clear();
            state.last_use_nano_mode = use_nano_mode;
            let _ = gather_data(true, use_nano_mode, &mut state, &user_map, clk_tck, num_cpus);
            thread::sleep(Duration::from_millis(100));
        }

        let do_mem = mem_override_shared.load(Ordering::Relaxed) || state.last_mem_update.elapsed().as_secs_f64() >= 2.0;
        if do_mem {
            state.last_mem_update = Instant::now();
        }

        let (procs, sys) = gather_data(do_mem, use_nano_mode, &mut state, &user_map, clk_tck, num_cpus);
        
        state.last_procs.clear();
        for p in &procs {
            state.last_procs.insert(p.pid, p.clone());
        }

        let _ = tx.send((procs, sys));

        let elapsed = start.elapsed();
        let sleep_time = int_sec - elapsed.as_secs_f64();
        let start_mem_ov = mem_override_shared.load(Ordering::Relaxed);
        
        if sleep_time > 0.0 {
            let mut remaining = sleep_time;
            while remaining > 0.0 {
                let chunk = remaining.min(0.05);
                thread::sleep(Duration::from_secs_f64(chunk));
                
                let new_int = f64::from_bits(interval_shared.load(Ordering::Relaxed));
                let new_mem_ov = mem_override_shared.load(Ordering::Relaxed);
                
                if new_int != int_sec || new_mem_ov != start_mem_ov {
                    break;
                }
                remaining -= chunk;
            }
        } else {
            thread::yield_now();
        }
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

    let interval_shared = Arc::new(AtomicU64::new(interval.to_bits()));
    let mem_override_shared = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();

    let user_map = build_user_map();
    let clk_tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as u64;
    let num_cpus = num_cpus::get() as u64;

    thread::spawn({
        let interval_shared = Arc::clone(&interval_shared);
        let mem_override_shared = Arc::clone(&mem_override_shared);
        let user_map = user_map.clone();
        move || {
            worker_loop(tx, interval_shared, mem_override_shared, user_map, clk_tck, num_cpus);
        }
    });

    let _guard = TerminalGuard::new()?;
    let mut app = App::new(interval, no_color);
    
    app.render()?; 

    let mut last_render = Instant::now();
    let mut current_data = (Vec::new(), SystemInfo::default());
    let mut ui_dirty = true;

    'outer: loop {
        let poll_timeout = Duration::from_millis(16);
        if event::poll(poll_timeout)? {
            let mut keys = Vec::new();
            loop {
                if let Event::Key(k) = event::read()? { keys.push(k); }
                if !event::poll(Duration::ZERO)? { break; }
            }
            for key in keys {
                // ── KILL CONFIRMATION MODE ──
                if app.kill_confirming {
                    match key.code {
                        KeyCode::Char('y') | KeyCode::Char('Y') => {
                            for &pid in &app.marked_pids {
                                unsafe { libc::kill(pid, libc::SIGTERM); }
                            }
                            app.marked_pids.clear();
                            app.kill_mode = false;
                            app.kill_confirming = false;
                            ui_dirty = true;
                        }
                        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                            app.kill_confirming = false;
                            app.kill_mode = false;
                            app.marked_pids.clear();
                            ui_dirty = true;
                        }
                        _ => {}
                    }
                    continue;
                }

                // ── KILL SELECTION MODE ──
                if app.kill_mode {
                    match key.code {
                        KeyCode::Esc => {
                            app.kill_mode = false;
                            app.marked_pids.clear();
                            ui_dirty = true;
                            continue;
                        }
                        KeyCode::Up => {
                            app.kill_selected = app.kill_selected.saturating_sub(1);
                            ui_dirty = true;
                            continue;
                        }
                        KeyCode::Down => {
                            let max_idx = app.filtered_raw.len().saturating_sub(1);
                            app.kill_selected = (app.kill_selected + 1).min(max_idx);
                            ui_dirty = true;
                            continue;
                        }
                        KeyCode::Char(' ') => {
                            if app.kill_selected < app.filtered_raw.len() {
                                let pid = app.filtered_raw[app.kill_selected].pid;
                                if app.marked_pids.contains(&pid) {
                                    app.marked_pids.remove(&pid);
                                } else {
                                    app.marked_pids.insert(pid);
                                }
                                ui_dirty = true;
                            }
                            continue;
                        }
                        KeyCode::Enter => {
                            if !app.marked_pids.is_empty() {
                                app.kill_confirming = true;
                                ui_dirty = true;
                            }
                            continue;
                        }
                        _ => continue,
                    }
                }

                // ── SEARCH MODE ──
                if app.searching {
                    match key.code {
                        KeyCode::Esc => { app.filter_string.clear(); app.searching=false; ui_dirty = true; continue; }
                        KeyCode::Enter => { app.searching=false; ui_dirty = true; continue; }
                        KeyCode::Backspace|KeyCode::Delete => { app.filter_string.pop(); ui_dirty = true; continue; }
                        KeyCode::Char(c) if c.is_ascii_graphic()||c==' ' => { app.filter_string.push(c); ui_dirty = true; continue; }
                        _ => continue,
                    }
                }

                // ── NORMAL MODE ──
                match key.code {
                    KeyCode::Char('q')|KeyCode::Char('Q') => break 'outer,
                    KeyCode::Char('/') => { app.searching=true; ui_dirty = true; continue; }
                    KeyCode::Char('k')|KeyCode::Char('K') => {
                        if !app.filtered_raw.is_empty() {
                            app.kill_mode = true;
                            app.kill_selected = 0;
                            app.marked_pids.clear();
                            app.kill_confirming = false;
                            ui_dirty = true;
                        }
                        continue;
                    }
                    KeyCode::Char('m')|KeyCode::Char('M') => { 
                        app.mem_override = !app.mem_override; 
                        mem_override_shared.store(app.mem_override, Ordering::Relaxed); 
                        ui_dirty = true; 
                    }
                    KeyCode::Esc => { app.filter_string.clear(); ui_dirty = true; continue; }
                    KeyCode::Up => { app.scroll_pos = app.scroll_pos.saturating_sub(5); ui_dirty = true; }
                    KeyCode::Down => {
                        let vis = app.term_height.saturating_sub(3);
                        let max = if app.data_rows.len()>vis { app.data_rows.len()-vis } else { 0 };
                        app.scroll_pos = (app.scroll_pos+5).min(max); ui_dirty = true;
                    }
                    KeyCode::Left => { app.cmd_offset = app.cmd_offset.saturating_sub(40); app.soft_update(); ui_dirty = true; continue; }
                    KeyCode::Right => { app.cmd_offset = (app.cmd_offset+40).min(2000); app.soft_update(); ui_dirty = true; continue; }
                    KeyCode::Char('+')|KeyCode::Char('=') => {
                        if let Some(pos) = intervals.iter().position(|&v| v==app.interval) {
                            if pos < intervals.len()-1 { 
                                app.interval = intervals[pos+1]; 
                                interval_shared.store(app.interval.to_bits(), Ordering::Relaxed);
                                ui_dirty = true; 
                            }
                        }
                    }
                    KeyCode::Char('-') => {
                        if let Some(pos) = intervals.iter().position(|&v| v==app.interval) {
                            if pos > 0 { 
                                app.interval = intervals[pos-1]; 
                                interval_shared.store(app.interval.to_bits(), Ordering::Relaxed);
                                ui_dirty = true; 
                            }
                        }
                    }
                    KeyCode::Char(c) if c.is_ascii_digit() => {
                        let n = c.to_digit(10).unwrap() as usize;
                        if (1..=8).contains(&n) {
                            let col = n-1;
                            if col == app.sort_col { app.sort_reverse = !app.sort_reverse; }
                            else { app.sort_col = col; app.sort_reverse = col>2; }
                            app.soft_update(); app.scroll_pos=0; ui_dirty = true;
                        }
                    }
                    _ => {}
                }
            }
        }

        let mut got_new_data = false;
        while let Ok(new_data) = rx.try_recv() {
            current_data = new_data;
            got_new_data = true;
        }

        let data_frozen = app.kill_mode || app.kill_confirming;
        let update_data = got_new_data && !data_frozen;

        let render_interval = if app.searching || app.kill_mode || app.kill_confirming { 0.1 } else { app.interval };
        let time_since_render = last_render.elapsed().as_secs_f64();
        
        if update_data || ui_dirty || time_since_render >= render_interval {
            if update_data {
                app.raw_data = current_data.0.clone();
                app.system_info = current_data.1.clone();
            }
            app.apply_filter_and_sort();
            app.compute_widths();
            
            if app.kill_mode && !app.filtered_raw.is_empty() {
                app.kill_selected = app.kill_selected.min(app.filtered_raw.len() - 1);
                let visible = app.term_height.saturating_sub(3);
                if app.kill_selected < app.scroll_pos {
                    app.scroll_pos = app.kill_selected;
                } else if app.kill_selected >= app.scroll_pos + visible {
                    app.scroll_pos = app.kill_selected.saturating_sub(visible) + 1;
                }
            }
            
            let vis = app.term_height.saturating_sub(3);
            let max = if app.data_rows.len() > vis { app.data_rows.len() - vis } else { 0 };
            app.scroll_pos = app.scroll_pos.min(max);
            if app.searching { app.scroll_pos = 0; }
            
            app.render()?;
            last_render = Instant::now();
            ui_dirty = false;
        }

        if terminal::size().map(|(w,h)| w as usize != app.term_width || h as usize != app.term_height).unwrap_or(false) {
            let (w,h) = terminal::size().unwrap_or((80,24));
            app.term_width = w as usize; app.term_height = h as usize;
            app.compute_widths(); 
            app.render()?;
            last_render = Instant::now();
            ui_dirty = false;
        }
    }
    Ok(())
}
