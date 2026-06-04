use crossterm::{
    cursor,
    event::{self, Event, KeyCode},
    execute,
    terminal::{self, ClearType},
};
use std::{
    collections::{HashMap, HashSet},
    env, fs,
    io::{self, Write},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc,
    },
    thread,
    time::{Duration, Instant},
};

// ─ Terminal Guard: guarantees cleanup on exit, panic, or signal ──────
struct TerminalGuard {
    raw_enabled: bool,
    alt_screen_enabled: bool,
}

impl TerminalGuard {
    fn new() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let mut guard = Self {
            raw_enabled: true,
            alt_screen_enabled: false,
        };
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
const BLUE_BRIGHT: (u8, u8, u8) = (0, 128, 255);
const RED_DESAT: (u8, u8, u8) = (255, 60, 60);
const INDIGO_DESAT: (u8, u8, u8) = (111, 81, 161);
const WHITE_DIMMER: (u8, u8, u8) = (200, 200, 200);
const WHITE_PURE: (u8, u8, u8) = (255, 255, 255);
const CYAN_BRIGHT: (u8, u8, u8) = (0, 255, 255);
const GRAY_THREAD: (u8, u8, u8) = (120, 120, 120);
const GRAY_KERNEL: (u8, u8, u8) = (70, 70, 70);

fn format_memory(kib: u64, is_thread: bool) -> String {
    if is_thread {
        return "---".to_string();
    }
    if kib == 0 {
        return "0 KiB".to_string();
    }
    let mib = kib as f64 / 1024.0;
    if mib >= 1000.0 {
        format!("{} MiB", mib.round() as u64)
    } else if mib >= 100.0 {
        format!("{} MiB", (mib.round() as u64))
    } else if mib >= 10.0 {
        format!("{:.1} MiB", mib)
    } else if mib >= 1.0 {
        format!("{:.2} MiB", mib)
    } else {
        format!("{} KiB", kib)
    }
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
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    (
        ((r + m) * 255.0).round() as u8,
        ((g + m) * 255.0).round() as u8,
        ((b + m) * 255.0).round() as u8,
    )
}

fn ansi_fg_rgb(r: u8, g: u8, b: u8) -> String {
    format!("\x1b[38;2;{};{};{}m", r, g, b)
}
fn ansi_bg_rgb(r: u8, g: u8, b: u8) -> String {
    format!("\x1b[48;2;{};{};{}m", r, g, b)
}
fn ansi_reset() -> &'static str {
    "\x1b[0m"
}
fn color_text_rgb(r: u8, g: u8, b: u8, s: &str) -> String {
    format!("{}{}\x1b[39m", ansi_fg_rgb(r, g, b), s)
}
fn bold_text(s: &str) -> String {
    format!("\x1b[1m{}\x1b[22m", s)
}
fn bold_color_rgb(r: u8, g: u8, b: u8, s: &str) -> String {
    format!("\x1b[1m{}{}\x1b[39m\x1b[22m", ansi_fg_rgb(r, g, b), s)
}
fn visible_len(s: &str) -> usize {
    let mut len = 0;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.next_if_eq(&'[').is_some() {
            while chars.peek().map_or(false, |&d| d != 'm') {
                chars.next();
            }
            chars.next();
        } else {
            len += 1;
        }
    }
    len
}

// ── Raw /proc file reading helpers ─────────────────────────────────────
const PROC_BUF_SIZE: usize = 65536; // large enough for any /proc file (e.g., smaps_rollup)

/// Write a positive i32 as decimal digits into buf, returns number of bytes written.
fn write_i32(mut n: i32, buf: &mut [u8]) -> usize {
    if n == 0 {
        buf[0] = b'0';
        return 1;
    }
    let mut len = 0;
    // write digits in reverse (least significant first)
    while n > 0 {
        buf[len] = b'0' + (n % 10) as u8;
        len += 1;
        n /= 10;
    }
    // reverse in-place
    let (mut a, mut b) = (0, len - 1);
    while a < b {
        buf.swap(a, b);
        a += 1;
        b -= 1;
    }
    len
}

/// Read a whole file into `buf` using raw open/read/close. Returns slice of valid bytes.
unsafe fn proc_read_raw(path: *const libc::c_char, buf: &mut [u8]) -> io::Result<&[u8]> {
    let fd = libc::open(path, libc::O_RDONLY);
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let n = libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len());
    let err = if n < 0 {
        Some(io::Error::last_os_error())
    } else {
        None
    };
    libc::close(fd);
    if let Some(e) = err {
        return Err(e);
    }
    Ok(&buf[..n as usize])
}

/// Convenience: read a proc file into a string slice (assumes ASCII content).
fn proc_read_str(path: *const libc::c_char, buf: &mut [u8]) -> io::Result<&str> {
    let bytes = unsafe { proc_read_raw(path, buf)? };
    // All /proc files are ASCII, so this is safe
    Ok(unsafe { std::str::from_utf8_unchecked(bytes) })
}

/// Write a null-terminated path like /proc/12345/stat into `buf`.
/// Returns a C string pointer.
fn build_proc_path(pid: i32, suffix: &str, buf: &mut [u8]) -> *const libc::c_char {
    let mut pos = 0;
    let prefix = b"/proc/";
    buf[pos..pos + prefix.len()].copy_from_slice(prefix);
    pos += prefix.len();
    pos += write_i32(pid, &mut buf[pos..]);

    let suffix_bytes = suffix.as_bytes();
    buf[pos..pos + suffix_bytes.len()].copy_from_slice(suffix_bytes);
    pos += suffix_bytes.len();
    buf[pos] = 0;
    buf.as_ptr() as *const libc::c_char
}

/// Two-level path: /proc/{pid}/task/{tid}/{suffix} directly on stack
fn build_task_path(pid: i32, tid: i32, suffix: &str, buf: &mut [u8]) -> *const libc::c_char {
    let mut pos = 0;
    let prefix = b"/proc/";
    buf[pos..pos + prefix.len()].copy_from_slice(prefix);
    pos += prefix.len();

    pos += write_i32(pid, &mut buf[pos..]);

    let mid = b"/task/";
    buf[pos..pos + mid.len()].copy_from_slice(mid);
    pos += mid.len();

    pos += write_i32(tid, &mut buf[pos..]);

    let slash = b'/';
    buf[pos] = slash;
    pos += 1;

    let suffix_bytes = suffix.as_bytes();
    buf[pos..pos + suffix_bytes.len()].copy_from_slice(suffix_bytes);
    pos += suffix_bytes.len();
    buf[pos] = 0;
    buf.as_ptr() as *const libc::c_char
}

// ── Direct /proc parsers (now use reusable buffers) ───────────────────
fn read_meminfo_buf(buf: &mut [u8]) -> HashMap<String, u64> {
    let mut map = HashMap::new();
    let path = b"/proc/meminfo\0";
    let ptr = path.as_ptr() as *const libc::c_char;
    if let Ok(content) = proc_read_str(ptr, buf) {
        for line in content.lines() {
            let mut parts = line.splitn(2, ':');
            if let (Some(key), Some(val)) = (parts.next(), parts.next()) {
                if let Some(num) = val
                    .trim()
                    .split_whitespace()
                    .next()
                    .and_then(|s| s.parse().ok())
                {
                    map.insert(key.trim().to_string(), num);
                }
            }
        }
    }
    map
}

fn read_cpu_times_buf(buf: &mut [u8]) -> (u64, u64) {
    let path = b"/proc/stat\0";
    let ptr = path.as_ptr() as *const libc::c_char;
    if let Ok(content) = proc_read_str(ptr, buf) {
        if let Some(line) = content.lines().find(|l| l.starts_with("cpu ")) {
            let fields: Vec<u64> = line
                .split_whitespace()
                .skip(1)
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

// Tiny buffer just for the one-time startup read
fn read_meminfo_small() -> HashMap<String, u64> {
    let mut buf = [0u8; 4096];
    read_meminfo_buf(&mut buf)
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

// ── Process/Thread info struct ──────────────────────────────────────
#[derive(Clone)]
struct ProcInfo {
    pid: i32,
    tid: i32,
    ppid: i32,
    user: String,
    cmd: String,
    full_cmd: String,
    pss_kb: u64,
    uss_kb: u64,
    rss_kb: u64,
    swap_kb: u64,
    state: char,
    cpu_percent: f64,
    is_thread: bool,
    is_kernel: bool,
    tree_depth: u8,
    tree_mask: u32,
    is_last_child: bool,
}

fn color_pss(size_bytes: u64, limit_red_bytes: u64) -> (u8, u8, u8) {
    let green = (0, 255, 0);
    let yellow_pale = (255, 255, 230);
    let yellow_full = (255, 255, 0);
    let red = (255, 0, 0);
    let limit_yellow_pale = 64 * 1024u64.pow(2);
    let limit_yellow_full = 512 * 1024u64.pow(2);
    if size_bytes <= 0 {
        return green;
    }
    if size_bytes >= limit_red_bytes {
        return red;
    }
    if size_bytes <= limit_yellow_pale {
        return lerp_color(
            green,
            yellow_pale,
            size_bytes as f64 / limit_yellow_pale as f64,
        );
    }
    if size_bytes <= limit_yellow_full {
        return lerp_color(
            yellow_pale,
            yellow_full,
            (size_bytes - limit_yellow_pale) as f64
                / (limit_yellow_full - limit_yellow_pale) as f64,
        );
    }
    lerp_color(
        yellow_full,
        red,
        (size_bytes - limit_yellow_full) as f64 / (limit_red_bytes - limit_yellow_full) as f64,
    )
}

fn color_rss(size_bytes: u64, mem_total_bytes: u64) -> (u8, u8, u8) {
    let l1 = 8 * 1024u64.pow(2);
    let l2 = 10 * 1024u64.pow(2);
    let l3 = 128 * 1024u64.pow(2);
    let l4 = 136 * 1024u64.pow(2);
    let l5 = 1024 * 1024u64.pow(2);
    let l6 = 1072 * 1024u64.pow(2);
    let l7 = (mem_total_bytes as f64 * 0.97) as u64;
    let l8 = mem_total_bytes;
    if size_bytes <= l1 {
        if l1 == 0 {
            return (255, 0, 0);
        }
        return hsv_to_rgb((size_bytes as f64 / l1 as f64) * 360.0, 1.0, 1.0);
    } else if size_bytes <= l2 {
        let t = (size_bytes - l1) as f64 / (l2 - l1) as f64;
        return hsv_to_rgb(0.0, 1.0 - 0.2 * t, 1.0);
    } else if size_bytes <= l3 {
        let t = (size_bytes - l2) as f64 / (l3 - l2) as f64;
        return hsv_to_rgb(t * 360.0, 0.8, 1.0);
    } else if size_bytes <= l4 {
        let t = (size_bytes - l3) as f64 / (l4 - l3) as f64;
        return hsv_to_rgb(0.0, 0.8 - 0.3 * t, 1.0);
    } else if size_bytes <= l5 {
        let t = (size_bytes - l4) as f64 / (l5 - l4) as f64;
        return hsv_to_rgb(t * 360.0, 0.5, 1.0);
    } else if size_bytes <= l6 {
        let t = (size_bytes - l5) as f64 / (l6 - l5) as f64;
        return hsv_to_rgb(0.0, 0.5 - 0.3 * t, 1.0);
    } else if size_bytes <= l7 {
        let t = (size_bytes - l6) as f64 / (l7 - l6) as f64;
        return hsv_to_rgb(t * 360.0, 0.2, 1.0);
    } else if size_bytes <= l8 {
        return lerp_color(
            (255, 204, 204),
            (128, 128, 128),
            (size_bytes - l7) as f64 / (l8 - l7) as f64,
        );
    }
    (128, 128, 128)
}

fn color_swap(size_bytes: u64, swap_total_bytes: u64) -> (u8, u8, u8) {
    let red = (255, 0, 0);
    let pink = (255, 128, 128);
    let limit_pink = 1 * 1024u64.pow(3);
    if size_bytes >= swap_total_bytes {
        return red;
    }
    if size_bytes >= limit_pink {
        return lerp_color(
            pink,
            red,
            (size_bytes - limit_pink) as f64 / (swap_total_bytes - limit_pink) as f64,
        );
    }
    if size_bytes > 0 {
        return lerp_color(WHITE_PURE, pink, size_bytes as f64 / limit_pink as f64);
    }
    WHITE_PURE
}

fn color_cpu(percent: f64) -> (u8, u8, u8) {
    let p = percent.clamp(0.0, 100.0) / 100.0;
    if p < 0.5 {
        lerp_color((0, 255, 0), (255, 255, 0), p * 2.0)
    } else {
        lerp_color((255, 255, 0), (255, 0, 0), (p - 0.5) * 2.0)
    }
}

struct App {
    term_width: usize,
    term_height: usize,
    interval: f64,
    no_color: bool,
    current_user: String,
    mem_total_bytes: u64,
    limit_red_bytes: u64,
    swap_total_bytes: u64,
    raw_data: Vec<ProcInfo>,
    filtered_raw: Vec<ProcInfo>,
    widths: Vec<usize>,
    sort_col: usize,
    sort_reverse: bool,
    filter_string: String,
    searching: bool,
    kill_mode: bool,
    kill_selected: usize,
    marked_pids: HashSet<i32>,
    kill_confirming: bool,
    cmd_offset: usize,
    tree_mode: bool,
    thread_mode: bool,
    show_kernel: bool,
    collapsed_pids: HashSet<i32>,
    system_info: SystemInfo,
    scroll_pos: usize,
    mem_override: bool,
}

#[derive(Default, Clone)]
struct SystemInfo {
    cpu_percent: f64,
    mem_percent: f64,
    swap_percent: f64,
    load: String,
    uptime: String,
    tasks: String,
}

impl App {
    fn new(interval: f64, no_color: bool) -> Self {
        let (w, h) = terminal::size().unwrap_or((80, 24));
        let meminfo = read_meminfo_small();
        let mem_total_bytes = *meminfo.get("MemTotal").unwrap_or(&(16 * 1024 * 1024)) * 1024;
        let swap_total_bytes = *meminfo.get("SwapTotal").unwrap_or(&(32 * 1024 * 1024)) * 1024;
        let limit_red_bytes = (mem_total_bytes as f64 * 0.99) as u64;
        let current_user = env::var("SUDO_USER")
            .ok()
            .or_else(|| env::var("USER").ok())
            .unwrap_or_default();
        Self {
            term_width: w as usize,
            term_height: h as usize,
            interval,
            no_color,
            current_user,
            mem_total_bytes,
            limit_red_bytes,
            swap_total_bytes,
            raw_data: vec![],
            filtered_raw: vec![],
            widths: vec![0; 8],
            sort_col: 3,
            sort_reverse: true,
            filter_string: String::new(),
            searching: false,
            kill_mode: false,
            kill_selected: 0,
            marked_pids: HashSet::new(),
            kill_confirming: false,
            cmd_offset: 0,
            tree_mode: false,
            thread_mode: false,
            show_kernel: false,
            collapsed_pids: HashSet::new(),
            system_info: SystemInfo::default(),
            scroll_pos: 0,
            mem_override: false,
        }
    }

    fn mark_and_advance(&mut self) {
        if self.kill_selected < self.filtered_raw.len() {
            let pid = if self.filtered_raw[self.kill_selected].is_thread {
                self.filtered_raw[self.kill_selected].tid
            } else {
                self.filtered_raw[self.kill_selected].pid
            };
            self.marked_pids.insert(pid);

            let max_idx = self.filtered_raw.len().saturating_sub(1);
            self.kill_selected = (self.kill_selected + 1).min(max_idx);

            let visible = self.term_height.saturating_sub(3);
            if self.kill_selected >= self.scroll_pos + visible {
                self.scroll_pos = self.kill_selected.saturating_sub(visible) + 1;
            }
        }
    }

    fn apply_filter_and_sort(&mut self) {
        if !self.tree_mode && self.filter_string.is_empty() {
            self.filtered_raw = self.raw_data.clone();
            match self.sort_col {
                0 => self.filtered_raw.sort_by_key(|p| p.pid),
                1 => self.filtered_raw.sort_by(|a, b| {
                    a.user
                        .chars()
                        .map(|c| c.to_ascii_lowercase())
                        .cmp(b.user.chars().map(|c| c.to_ascii_lowercase()))
                }),
                2 => self.filtered_raw.sort_by(|a, b| {
                    a.cmd
                        .chars()
                        .map(|c| c.to_ascii_lowercase())
                        .cmp(b.cmd.chars().map(|c| c.to_ascii_lowercase()))
                }),
                3 => self.filtered_raw.sort_by_key(|p| p.pss_kb),
                4 => self.filtered_raw.sort_by_key(|p| p.uss_kb),
                5 => self.filtered_raw.sort_by_key(|p| p.rss_kb),
                6 => self.filtered_raw.sort_by_key(|p| p.swap_kb),
                7 => self.filtered_raw.sort_by(|a, b| {
                    a.cpu_percent
                        .partial_cmp(&b.cpu_percent)
                        .unwrap_or(std::cmp::Ordering::Equal)
                }),
                _ => {}
            }
            if self.sort_reverse {
                self.filtered_raw.reverse();
            }
        } else if !self.tree_mode && !self.filter_string.is_empty() {
            let lower = self.filter_string.to_lowercase();
            self.filtered_raw = self
                .raw_data
                .iter()
                .filter(|p| {
                    p.full_cmd.to_lowercase().contains(&lower)
                        || p.pid.to_string().contains(&lower)
                        || p.user.to_lowercase().contains(&lower)
                })
                .cloned()
                .collect();

            match self.sort_col {
                0 => self.filtered_raw.sort_by_key(|p| p.pid),
                1 => self.filtered_raw.sort_by(|a, b| {
                    a.user
                        .chars()
                        .map(|c| c.to_ascii_lowercase())
                        .cmp(b.user.chars().map(|c| c.to_ascii_lowercase()))
                }),
                2 => self.filtered_raw.sort_by(|a, b| {
                    a.cmd
                        .chars()
                        .map(|c| c.to_ascii_lowercase())
                        .cmp(b.cmd.chars().map(|c| c.to_ascii_lowercase()))
                }),
                3 => self.filtered_raw.sort_by_key(|p| p.pss_kb),
                4 => self.filtered_raw.sort_by_key(|p| p.uss_kb),
                5 => self.filtered_raw.sort_by_key(|p| p.rss_kb),
                6 => self.filtered_raw.sort_by_key(|p| p.swap_kb),
                7 => self.filtered_raw.sort_by(|a, b| {
                    a.cpu_percent
                        .partial_cmp(&b.cpu_percent)
                        .unwrap_or(std::cmp::Ordering::Equal)
                }),
                _ => {}
            }
            if self.sort_reverse {
                self.filtered_raw.reverse();
            }
        } else {
            let lower = self.filter_string.to_lowercase();
            let matches_filter = |p: &ProcInfo| -> bool {
                if lower.is_empty() {
                    return true;
                }
                p.full_cmd.to_lowercase().contains(&lower) || p.pid.to_string().contains(&lower)
            };

            let mut children_map: HashMap<i32, Vec<i32>> = HashMap::new();
            let mut proc_map: HashMap<i32, &ProcInfo> = HashMap::new();
            let all_pids: HashSet<i32> = self
                .raw_data
                .iter()
                .filter(|p| !p.is_thread)
                .map(|p| p.pid)
                .collect();
            let mut all_roots: Vec<i32> = Vec::new();

            for p in &self.raw_data {
                if p.is_thread {
                    continue;
                }
                proc_map.insert(p.pid, p);
                children_map.entry(p.ppid).or_default().push(p.pid);
                if p.ppid == 0 || !all_pids.contains(&p.ppid) {
                    all_roots.push(p.pid);
                }
            }

            for children in children_map.values_mut() {
                children.sort();
            }

            let mut marked = HashSet::new();
            let mut ancestors: HashMap<i32, i32> = HashMap::new();
            for p in &self.raw_data {
                if p.is_thread {
                    continue;
                }
                if matches_filter(p) {
                    marked.insert(p.pid);
                }
                ancestors.insert(p.pid, p.ppid);
            }

            if !self.filter_string.is_empty() && self.tree_mode {
                let mut queue: Vec<i32> = marked.iter().copied().collect();
                while let Some(pid) = queue.pop() {
                    if let Some(ppid) = ancestors.get(&pid) {
                        if all_pids.contains(ppid) && !marked.contains(ppid) {
                            marked.insert(*ppid);
                            queue.push(*ppid);
                        }
                    }
                }
            }

            let mut result = Vec::new();
            let mut visited = HashSet::new();
            let all_threads: Vec<&ProcInfo> =
                self.raw_data.iter().filter(|p| p.is_thread).collect();

            fn dfs(
                pid: i32,
                depth: u8,
                mask: u32,
                is_last: bool,
                children_map: &HashMap<i32, Vec<i32>>,
                proc_map: &HashMap<i32, &ProcInfo>,
                marked: &HashSet<i32>,
                collapsed: &HashSet<i32>,
                result: &mut Vec<ProcInfo>,
                visited: &mut HashSet<i32>,
                thread_mode: bool,
                all_threads: &[&ProcInfo],
            ) {
                if visited.contains(&pid) || !marked.contains(&pid) {
                    return;
                }
                visited.insert(pid);

                if let Some(&proc) = proc_map.get(&pid) {
                    let mut proc_with_info = proc.clone();
                    proc_with_info.tree_depth = depth;
                    proc_with_info.tree_mask = mask;
                    proc_with_info.is_last_child = is_last;
                    result.push(proc_with_info);
                }

                if collapsed.contains(&pid) {
                    return;
                }

                let child_mask = if !is_last {
                    mask | (1u32 << depth)
                } else {
                    mask
                };
                let threads: Vec<&ProcInfo> = all_threads
                    .iter()
                    .filter(|t| t.ppid == pid)
                    .copied()
                    .collect();
                let has_threads = !threads.is_empty();

                if let Some(children) = children_map.get(&pid) {
                    for (i, &child) in children.iter().enumerate() {
                        let child_is_last = (i == children.len() - 1) && !has_threads;
                        dfs(
                            child,
                            depth + 1,
                            child_mask,
                            child_is_last,
                            children_map,
                            proc_map,
                            marked,
                            collapsed,
                            result,
                            visited,
                            thread_mode,
                            all_threads,
                        );
                    }
                }

                if thread_mode && has_threads {
                    let mut sorted_threads = threads;
                    sorted_threads.sort_by_key(|t| t.tid);
                    for (i, t) in sorted_threads.iter().enumerate() {
                        let thread_is_last = i == sorted_threads.len() - 1;
                        let mut thread_with_info = (*t).clone();
                        thread_with_info.tree_depth = depth + 1;
                        thread_with_info.tree_mask = child_mask;
                        thread_with_info.is_last_child = thread_is_last;
                        result.push(thread_with_info);
                    }
                }
            }

            for (i, &root) in all_roots.iter().enumerate() {
                if marked.contains(&root) || !visited.contains(&root) {
                    let root_is_last = i == all_roots.len() - 1;
                    dfs(
                        root,
                        0,
                        0,
                        root_is_last,
                        &children_map,
                        &proc_map,
                        &marked,
                        &self.collapsed_pids,
                        &mut result,
                        &mut visited,
                        self.thread_mode,
                        &all_threads,
                    );
                }
            }

            self.filtered_raw = result;
        }
    }

    fn format_row(&self, p: &ProcInfo) -> Vec<String> {
        let indent = if self.tree_mode && p.tree_depth > 0 {
            let mut s = String::with_capacity(p.tree_depth as usize * 3 + 2);
            for i in 0..p.tree_depth {
                if (p.tree_mask & (1u32 << i)) != 0 {
                    s.push_str("│  ");
                } else {
                    s.push_str("   ");
                }
            }
            s + if p.is_last_child {
                "└─ "
            } else {
                "├─ "
            }
        } else {
            String::new()
        };

        let chars: Vec<char> = p.full_cmd.chars().collect();
        let max_offset = chars.len().saturating_sub(40);
        let offset = self.cmd_offset.min(max_offset);
        let displayed: String = chars.iter().skip(offset).take(40).collect();
        let cmd_str = format!("{}{: <40}", indent, displayed);

        let pid_str = if p.is_thread {
            format!("{}", p.tid)
        } else {
            format!("{}", p.pid)
        };
        let user = &p.user;

        let pid_cell = if self.no_color {
            pid_str.clone()
        } else if p.is_kernel {
            color_text_rgb(GRAY_KERNEL.0, GRAY_KERNEL.1, GRAY_KERNEL.2, &pid_str)
        } else {
            let col = if p.is_thread {
                GRAY_THREAD
            } else {
                YELLOW_BRIGHT
            };
            color_text_rgb(col.0, col.1, col.2, &pid_str)
        };
        let user_cell = if self.no_color {
            user.clone()
        } else if p.is_kernel {
            color_text_rgb(GRAY_KERNEL.0, GRAY_KERNEL.1, GRAY_KERNEL.2, user)
        } else {
            let (r, g, b) = if *user == self.current_user {
                BLUE_BRIGHT
            } else if user == "root" {
                RED_DESAT
            } else {
                INDIGO_DESAT
            };
            color_text_rgb(r, g, b, user)
        };
        let cmd_cell = if self.no_color {
            cmd_str.clone()
        } else if p.is_kernel {
            color_text_rgb(GRAY_KERNEL.0, GRAY_KERNEL.1, GRAY_KERNEL.2, &cmd_str)
        } else {
            color_text_rgb(WHITE_DIMMER.0, WHITE_DIMMER.1, WHITE_DIMMER.2, &cmd_str)
        };

        let pss = format_memory(p.pss_kb, p.is_thread);
        let uss = format_memory(p.uss_kb, p.is_thread);
        let rss = format_memory(p.rss_kb, p.is_thread);
        let swap = format_memory(p.swap_kb, p.is_thread);
        let cpu = format!("{:5.1}", p.cpu_percent);

        let pss_cell = if self.no_color {
            pss.clone()
        } else if p.is_kernel {
            color_text_rgb(GRAY_KERNEL.0, GRAY_KERNEL.1, GRAY_KERNEL.2, &pss)
        } else {
            if p.is_thread {
                color_text_rgb(GRAY_THREAD.0, GRAY_THREAD.1, GRAY_THREAD.2, &pss)
            } else {
                let (r, g, b) = color_pss(p.pss_kb * 1024, self.limit_red_bytes);
                color_text_rgb(r, g, b, &pss)
            }
        };
        let uss_cell = if self.no_color {
            uss.clone()
        } else if p.is_kernel {
            color_text_rgb(GRAY_KERNEL.0, GRAY_KERNEL.1, GRAY_KERNEL.2, &uss)
        } else {
            if p.is_thread {
                color_text_rgb(GRAY_THREAD.0, GRAY_THREAD.1, GRAY_THREAD.2, &uss)
            } else {
                let (r, g, b) = color_pss(p.uss_kb * 1024, self.limit_red_bytes);
                color_text_rgb(r, g, b, &uss)
            }
        };
        let rss_cell = if self.no_color {
            rss.clone()
        } else if p.is_kernel {
            color_text_rgb(GRAY_KERNEL.0, GRAY_KERNEL.1, GRAY_KERNEL.2, &rss)
        } else {
            if p.is_thread {
                color_text_rgb(GRAY_THREAD.0, GRAY_THREAD.1, GRAY_THREAD.2, &rss)
            } else {
                let (r, g, b) = color_rss(p.rss_kb * 1024, self.mem_total_bytes);
                color_text_rgb(r, g, b, &rss)
            }
        };
        let swap_cell = if self.no_color {
            swap.clone()
        } else if p.is_kernel {
            color_text_rgb(GRAY_KERNEL.0, GRAY_KERNEL.1, GRAY_KERNEL.2, &swap)
        } else {
            if p.is_thread {
                color_text_rgb(GRAY_THREAD.0, GRAY_THREAD.1, GRAY_THREAD.2, &swap)
            } else {
                let (r, g, b) = color_swap(p.swap_kb * 1024, self.swap_total_bytes);
                color_text_rgb(r, g, b, &swap)
            }
        };
        let cpu_cell = if self.no_color {
            cpu.clone()
        } else if p.is_kernel {
            color_text_rgb(GRAY_KERNEL.0, GRAY_KERNEL.1, GRAY_KERNEL.2, &cpu)
        } else {
            let (r, g, b) = color_cpu(p.cpu_percent);
            color_text_rgb(r, g, b, &cpu)
        };

        vec![
            pid_cell, user_cell, cmd_cell, pss_cell, uss_cell, rss_cell, swap_cell, cpu_cell,
        ]
    }

    fn compute_widths(&mut self) {
        let headers = [
            "1 PID",
            "2 User",
            "3 Command/Program",
            "4 PSS",
            "5 USS",
            "6 RSS",
            "7 Swap",
            "8 CPU%",
        ];
        self.widths = headers
            .iter()
            .map(|h| h.chars().count())
            .collect::<Vec<_>>();

        for p in &self.filtered_raw {
            let pid_len = if p.is_thread {
                p.tid.to_string().len()
            } else {
                p.pid.to_string().len()
            };
            self.widths[0] = self.widths[0].max(pid_len);

            self.widths[1] = self.widths[1].max(p.user.chars().count());

            let indent_len = if self.tree_mode && p.tree_depth > 0 {
                p.tree_depth as usize * 3 + 2
            } else {
                0
            };
            let chars_count = p.full_cmd.chars().count();
            let max_offset = chars_count.saturating_sub(40);
            let offset = self.cmd_offset.min(max_offset);
            let displayed_len = p.full_cmd.chars().skip(offset).take(40).count();
            self.widths[2] = self.widths[2].max(indent_len + displayed_len.max(40));

            self.widths[3] = self.widths[3].max(format_memory(p.pss_kb, p.is_thread).len());
            self.widths[4] = self.widths[4].max(format_memory(p.uss_kb, p.is_thread).len());
            self.widths[5] = self.widths[5].max(format_memory(p.rss_kb, p.is_thread).len());
            self.widths[6] = self.widths[6].max(format_memory(p.swap_kb, p.is_thread).len());
            self.widths[7] = self.widths[7].max(format!("{:5.1}", p.cpu_percent).len());
        }
    }

    fn build_header_line(&self) -> String {
        let headers = [
            "1 PID",
            "2 User",
            "3 Command/Program",
            "4 PSS",
            "5 USS",
            "6 RSS",
            "7 Swap",
            "8 CPU%",
        ];
        let gaps = [2, 1, 2, 2, 2, 2, 2];
        let aligns = [
            "right", "left", "left", "right", "right", "right", "right", "right",
        ];

        let mut line = String::new();
        for (i, label) in headers.iter().enumerate() {
            let width = self.widths[i];
            let label_len = label.chars().count();
            let pad = width.saturating_sub(label_len);

            let (left_pad, right_pad) = if aligns[i] == "right" {
                (pad, 0)
            } else {
                (0, pad)
            };

            line.push_str(&" ".repeat(left_pad));

            let fmt = if i == self.sort_col && !self.no_color {
                bold_color_rgb(CYAN_BRIGHT.0, CYAN_BRIGHT.1, CYAN_BRIGHT.2, label)
            } else if !self.no_color {
                bold_color_rgb(255, 255, 255, label)
            } else {
                bold_text(label)
            };

            line.push_str(&fmt);
            line.push_str(&" ".repeat(right_pad));

            if i < gaps.len() {
                line.push_str(&" ".repeat(gaps[i]));
            }
        }

        let max = self.term_width.saturating_sub(2);
        let vis_len = visible_len(&line);
        if vis_len < max {
            line.push_str(&" ".repeat(max - vis_len));
        }
        line.push_str(&format!("{}\x1b[0m  ", ansi_bg_rgb(40, 40, 40)));
        line
    }

    fn pad_cell(cell: &str, width: usize, align: &str) -> String {
        let v = visible_len(cell);
        if v >= width {
            return cell.to_string();
        }
        let pad = " ".repeat(width - v);
        if align == "right" {
            format!("{}{}", pad, cell)
        } else {
            format!("{}{}", cell, pad)
        }
    }

    fn build_system_line(&self) -> String {
        let si = &self.system_info;
        let bar = |pct: f64, w: usize| -> String {
            let filled = ((pct / 100.0) * w as f64).round() as usize;
            let filled = filled.min(w);
            if self.no_color {
                format!("{}{}", "|".repeat(filled), " ".repeat(w - filled))
            } else {
                let (r, g, b) = color_cpu(pct);
                format!(
                    "{}{}{}{}",
                    ansi_fg_rgb(r, g, b),
                    "|".repeat(filled),
                    "\x1b[39m",
                    " ".repeat(w - filled)
                )
            }
        };
        let cpu = format!(
            "{} {} {:5.1}%",
            bold_text("CPU"),
            bar(si.cpu_percent, 8),
            si.cpu_percent
        );
        let mem = format!(
            "{} {} {:5.1}%",
            bold_text("Mem"),
            bar(si.mem_percent, 8),
            si.mem_percent
        );
        let swp = format!(
            "{} {} {:5.1}%",
            bold_text("Swp"),
            bar(si.swap_percent, 4),
            si.swap_percent
        );
        let mut line = format!("{} {} {}", cpu, mem, swp);
        let try_append = |line: &mut String, prefix: &str, val: &str| {
            if val.is_empty() {
                return;
            }
            let candidate = format!(" {} {} {}", line, bold_text(prefix), val);
            if visible_len(&candidate) <= self.term_width {
                *line = candidate;
            }
        };
        try_append(&mut line, "Load 1/5/15min", &si.load);
        try_append(&mut line, "Uptime", &si.uptime);
        try_append(&mut line, "Tasks:", &si.tasks);
        let vis = visible_len(&line);
        if vis < self.term_width {
            line.push_str(&" ".repeat(self.term_width - vis));
        } else {
            while visible_len(&line) > self.term_width {
                line.pop();
            }
            line.push_str(&" ".repeat(self.term_width - visible_len(&line)));
        }
        line
    }

    fn render(&mut self) -> io::Result<()> {
        let total = self.filtered_raw.len();
        let visible = self.term_height.saturating_sub(3);
        let max_scroll = if total > visible { total - visible } else { 0 };
        self.scroll_pos = self.scroll_pos.min(max_scroll);

        let sys = self.build_system_line();
        let header = self.build_header_line();
        let gaps = [2, 1, 2, 2, 2, 2, 2];
        let aligns = [
            "right", "left", "left", "right", "right", "right", "right", "right",
        ];
        let track = ansi_bg_rgb(40, 40, 40);
        let thumb = ansi_bg_rgb(120, 120, 120);
        let rst = ansi_reset();

        let mut out = io::stdout().lock();
        execute!(out, cursor::MoveTo(0, 0))?;

        write!(out, "{}\r\n", sys)?;
        write!(out, "{}\r\n", header)?;

        for idx in 0..visible {
            let didx = self.scroll_pos + idx;
            let row = if didx < total {
                let formatted_row = self.format_row(&self.filtered_raw[didx]);
                let cells: Vec<String> = formatted_row
                    .iter()
                    .enumerate()
                    .map(|(i, c)| Self::pad_cell(c, self.widths[i], aligns[i]))
                    .collect();
                let mut line = String::new();
                for (i, c) in cells.iter().enumerate() {
                    line.push_str(c);
                    if i < gaps.len() {
                        line.push_str(&" ".repeat(gaps[i]));
                    }
                }

                if self.kill_mode && didx < self.filtered_raw.len() {
                    let pid = if self.filtered_raw[didx].is_thread {
                        self.filtered_raw[didx].tid
                    } else {
                        self.filtered_raw[didx].pid
                    };
                    let is_marked = self.marked_pids.contains(&pid);
                    let is_selected = didx == self.kill_selected;
                    let marker = if is_selected && is_marked {
                        "[*] "
                    } else if is_marked {
                        "[x] "
                    } else if is_selected {
                        "[ ] "
                    } else {
                        "    "
                    };

                    if is_selected {
                        line = format!("\x1b[7m{}{}\x1b[27m", marker, line);
                    } else {
                        line = format!("{}{}", marker, line);
                    }
                }

                let maxw = self.term_width.saturating_sub(2);
                let v = visible_len(&line);
                if v < maxw {
                    line.push_str(&" ".repeat(maxw - v));
                }
                line
            } else {
                " ".repeat(self.term_width.saturating_sub(2))
            };
            let scroll = if total > visible && total > 0 {
                let thumb_h = std::cmp::max(
                    1,
                    (visible as f64 * visible as f64 / total as f64).round() as usize,
                );
                let thumb_top = if total > visible {
                    ((visible - thumb_h) as f64
                        * (self.scroll_pos as f64 / (total - visible) as f64))
                        .round() as usize
                } else {
                    0
                };
                if idx >= thumb_top && idx < thumb_top + thumb_h {
                    format!("{thumb}  {rst}")
                } else {
                    format!("{track}  {rst}")
                }
            } else {
                "  ".to_string()
            };
            write!(out, "{}{}\r\n", row, scroll)?;
        }

        let totals = self
            .filtered_raw
            .iter()
            .filter(|p| !p.is_thread)
            .fold((0u64, 0u64), |(pss, swp), p| {
                (pss + p.pss_kb, swp + p.swap_kb)
            });
        let tot_text = format!(
            "PSS: {}  Swap: {}",
            format_memory(totals.0, false),
            format_memory(totals.1, false)
        );

        let int_str = if self.interval.fract() == 0.0 {
            format!("{}s", self.interval as u64)
        } else {
            format!("{}s", self.interval)
        };
        let headers = [
            "1 PID",
            "2 User",
            "3 Command/Program",
            "4 PSS",
            "5 USS",
            "6 RSS",
            "7 Swap",
            "8 CPU%",
        ];
        let arrow = if self.sort_reverse { "↓" } else { "↑" };
        let sort = format!("{} {}", headers[self.sort_col], arrow);

        let cpu_mode = if self.interval < 0.5 {
            " [NANO CPU]"
        } else {
            ""
        };
        let kernel_tag = if self.show_kernel { "[KERNEL] " } else { "" };
        let mode_tags = format!(
            "{}{}",
            kernel_tag,
            if self.tree_mode && self.thread_mode {
                "[TREE+THREADS]"
            } else if self.tree_mode {
                "[TREE]"
            } else if self.thread_mode {
                "[THREADS]"
            } else {
                ""
            }
        );

        let left = if self.kill_confirming {
            format!(
                "Kill {} marked process(es)/thread(s)? y/n",
                self.marked_pids.len()
            )
        } else if self.kill_mode {
            let marked_count = self.marked_pids.len();
            format!("KILL MODE: ↑↓ select | Space mark/advance | Enter confirm | Esc cancel | Marked: {}", marked_count)
        } else if self.searching {
            format!("Search: {}_ (Enter accept, Esc clear)", self.filter_string)
        } else if !self.filter_string.is_empty() {
            format!(
                "Filter: '{}' | {}{} | Interval: {} | / search",
                self.filter_string, sort, cpu_mode, int_str
            )
        } else {
            format!("{}{}{} | +/- change | T tree | H threads | k kernel | m mem-override | q quit | F9 kill | ←→ cmd | ↑↓ scroll", mode_tags, cpu_mode, int_str)
        };

        let left = if self.cmd_offset > 0 {
            format!("{}  Cmd offset: {}", left, self.cmd_offset)
        } else {
            left
        };
        let left = if self.mem_override {
            format!("{} [MEM OVERRIDE]", left)
        } else {
            left
        };

        let status = if left.len() + tot_text.len() + 1 <= self.term_width {
            format!(
                "{}{}{}",
                left,
                " ".repeat(self.term_width - left.len() - tot_text.len() - 1),
                tot_text
            )
        } else {
            let trunc: String = left
                .chars()
                .take(self.term_width.saturating_sub(tot_text.len() + 1))
                .collect();
            format!("{} {}", trunc, tot_text)
        };
        let status_bg = if self.kill_confirming {
            ansi_bg_rgb(180, 40, 40)
        } else if self.kill_mode {
            ansi_bg_rgb(40, 60, 100)
        } else if self.searching {
            ansi_bg_rgb(80, 100, 40)
        } else if self.show_kernel {
            ansi_bg_rgb(50, 50, 60)
        } else {
            ansi_bg_rgb(60, 60, 60)
        };
        let status_line = if self.no_color {
            format!("{: <width$}", status, width = self.term_width)
        } else {
            format!(
                "{}{}{}{}",
                status_bg,
                ansi_fg_rgb(255, 255, 255),
                format!("{: <width$}", status, width = self.term_width),
                rst
            )
        };

        write!(out, "{}", status_line)?;
        execute!(out, terminal::Clear(ClearType::FromCursorDown))?;
        out.flush()?;
        Ok(())
    }
}

// ── Background Worker Logic ────────────────────────────────────────
struct WorkerState {
    prev_cpu_times: HashMap<i32, (f64, Instant)>,
    prev_sys_cpu: Option<(u64, u64)>,
    cached_static: HashMap<i32, (String, String, String)>,
    last_procs: HashMap<i32, ProcInfo>,
    last_mem_update: Instant,
    last_use_nano_mode: bool,
    // reusable buffers for raw I/O
    path_buf: [u8; 128],    // for build_proc_path
    file_buf: [u8; PROC_BUF_SIZE],
}

fn gather_data(
    do_mem: bool,
    use_nano_mode: bool,
    thread_mode: bool,
    show_kernel: bool,
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
                        // Build /proc/{pid}/stat path and read
                        let stat_ptr = build_proc_path(pid, "/stat", &mut state.path_buf);
                        let stat_content = match proc_read_str(stat_ptr, &mut state.file_buf) {
                            Ok(c) => c,
                            Err(_) => continue,
                        };

                        let (state_char, utime, stime) =
                            if let Some(rbracket) = stat_content.rfind(')') {
                                let fields: Vec<&str> =
                                    stat_content[rbracket + 2..].split_whitespace().collect();
                                if fields.len() >= 13 {
                                    (
                                        fields[0].chars().next().unwrap_or('?'),
                                        fields[11].parse::<u64>().unwrap_or(0),
                                        fields[12].parse::<u64>().unwrap_or(0),
                                    )
                                } else {
                                    continue;
                                }
                            } else {
                                continue;
                            };

                        let mut kthread_name = String::new();
                        if let (Some(open), Some(close)) =
                            (stat_content.find('('), stat_content.rfind(')'))
                        {
                            if close > open {
                                kthread_name = stat_content[open + 1..close].to_string();
                            }
                        }

                        // Read /proc/{pid}/status (only once) and extract ppid + uid
                        let status_ptr = build_proc_path(pid, "/status", &mut state.path_buf);
                        let status_content = proc_read_str(status_ptr, &mut state.file_buf)
                            .ok()
                            .unwrap_or("");

                        let ppid = status_content
                            .lines()
                            .find(|l| l.starts_with("PPid:"))
                            .and_then(|l| l.split_whitespace().nth(1))
                            .and_then(|s| s.parse::<i32>().ok())
                            .unwrap_or(1);

                        let uid = status_content
                            .lines()
                            .find(|l| l.starts_with("Uid:"))
                            .and_then(|l| l.split_whitespace().nth(1))
                            .and_then(|s| s.parse::<u32>().ok())
                            .unwrap_or(0);
                        // status_content reference ends here – buffer can now be reused

                        // Nano‑CPU mode (schedstat)
                        let mut use_nano_for_this_proc = use_nano_mode;
                        let mut runtime_ns = 0.0;

                        if use_nano_for_this_proc {
                            let task_dir = format!("/proc/{}/task", pid);
                            let mut sum_ns = 0.0;
                            let mut found_any = false;

                            if let Ok(tasks) = fs::read_dir(&task_dir) {
                                for task in tasks.flatten() {
                                    if let Some(tid_str) = task.file_name().to_str() {
                                        if let Ok(tid) = tid_str.parse::<i32>() {
                                            let sched_ptr = build_task_path(pid, tid, "/schedstat", &mut state.path_buf);
                                            if let Ok(content) = proc_read_str(sched_ptr, &mut state.file_buf) {
                                                if let Some(ns_str) = content.split_whitespace().next() {
                                                    if let Ok(ns) = ns_str.parse::<f64>() {
                                                        sum_ns += ns;
                                                        found_any = true;
                                                    }
                                                }
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

                        // Memory (smaps_rollup)
                        let (mut pss_kb, mut uss_kb, mut rss_kb, mut swap_kb) = (0, 0, 0, 0);
                        if do_mem {
                            let smaps_ptr = build_proc_path(pid, "/smaps_rollup", &mut state.path_buf);
                            if let Ok(content) = proc_read_str(smaps_ptr, &mut state.file_buf) {
                                let mut private_clean = 0u64;
                                let mut private_dirty = 0u64;
                                for line in content.lines() {
                                    let mut parts = line.split_whitespace();
                                    match parts.next() {
                                        Some("Pss:") => {
                                            pss_kb = parts
                                                .next()
                                                .and_then(|s| s.parse().ok())
                                                .unwrap_or(0)
                                        }
                                        Some("Rss:") => {
                                            rss_kb = parts
                                                .next()
                                                .and_then(|s| s.parse().ok())
                                                .unwrap_or(0)
                                        }
                                        Some("Private_Clean:") => {
                                            private_clean = parts
                                                .next()
                                                .and_then(|s| s.parse().ok())
                                                .unwrap_or(0)
                                        }
                                        Some("Private_Dirty:") => {
                                            private_dirty = parts
                                                .next()
                                                .and_then(|s| s.parse().ok())
                                                .unwrap_or(0)
                                        }
                                        Some("Swap:") => {
                                            swap_kb = parts
                                                .next()
                                                .and_then(|s| s.parse().ok())
                                                .unwrap_or(0)
                                        }
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

                        let is_kernel = rss_kb == 0 && !kthread_name.is_empty();
                        if rss_kb == 0 && !(is_kernel && show_kernel) {
                            continue;
                        }

                        // User and command line
                        let (user_str, cmd_str, full_cmd_str) = if is_kernel {
                            ("root".to_string(), kthread_name.clone(), kthread_name)
                        } else if let Some(cached) = state.cached_static.get(&pid) {
                            cached.clone()
                        } else {
                            let user = user_map
                                .get(&uid)
                                .cloned()
                                .unwrap_or_else(|| uid.to_string());

                            let cmdline_ptr = build_proc_path(pid, "/cmdline", &mut state.path_buf);
                            let full_cmd = proc_read_str(cmdline_ptr, &mut state.file_buf)
                                .unwrap_or("")
                                .replace('\0', " ")
                                .trim()
                                .to_string();
                            let full_cmd = full_cmd.chars().take(2040).collect::<String>();
                            let cmd = full_cmd.chars().take(40).collect::<String>();
                            let res = (user, cmd, full_cmd);
                            state.cached_static.insert(pid, res.clone());
                            res
                        };

                        let current_val = if use_nano_for_this_proc {
                            runtime_ns
                        } else {
                            (utime + stime) as f64
                        };
                        let mut cpu_percent = 0.0;

                        if let Some(&(prev_val, prev_time)) = state.prev_cpu_times.get(&pid) {
                            let delta = current_val - prev_val;
                            let dt = now.duration_since(prev_time).as_secs_f64();
                            if dt > 0.0 && delta >= 0.0 {
                                if use_nano_for_this_proc {
                                    cpu_percent =
                                        (delta / 1_000_000_000.0 / dt / num_cpus as f64) * 100.0;
                                } else {
                                    cpu_percent =
                                        (delta / clk_tck as f64 / dt / num_cpus as f64) * 100.0;
                                }
                            }
                        }
                        state.prev_cpu_times.insert(pid, (current_val, now));

                        procs.push(ProcInfo {
                            pid,
                            tid: pid,
                            ppid,
                            user: user_str.clone(),
                            cmd: cmd_str.clone(),
                            full_cmd: full_cmd_str.clone(),
                            pss_kb,
                            uss_kb,
                            rss_kb,
                            swap_kb,
                            state: state_char,
                            cpu_percent,
                            is_thread: false,
                            is_kernel,
                            tree_depth: 0,
                            tree_mask: 0,
                            is_last_child: false,
                        });

                        // Thread enumeration
                        if thread_mode {
                            let task_dir = format!("/proc/{}/task", pid);
                            if let Ok(tasks) = fs::read_dir(&task_dir) {
                                for task in tasks.flatten() {
                                    if let Some(tid_str) = task.file_name().to_str() {
                                        if let Ok(tid) = tid_str.parse::<i32>() {
                                            if tid == pid {
                                                continue;
                                            }
                                            let sched_ptr = build_task_path(pid, tid, "/schedstat", &mut state.path_buf);
                                            let mut t_cpu = 0.0;
                                            if let Ok(content) = proc_read_str(sched_ptr, &mut state.file_buf) {
                                                if let Some(ns_str) = content.split_whitespace().next() {
                                                    if let Ok(ns) = ns_str.parse::<f64>() {
                                                        let prev = state.prev_cpu_times.get(&tid).copied();
                                                        if let Some((prev_val, prev_time)) = prev {
                                                            let dt = now.duration_since(prev_time).as_secs_f64();
                                                            let delta = ns - prev_val;
                                                            if dt > 0.0 && delta >= 0.0 {
                                                                t_cpu = (delta / 1_000_000_000.0 / dt / num_cpus as f64) * 100.0;
                                                            }
                                                        }
                                                        state.prev_cpu_times.insert(tid, (ns, now));
                                                    }
                                                }
                                            }
                                            procs.push(ProcInfo {
                                                pid,
                                                tid,
                                                ppid: pid,
                                                user: user_str.clone(),
                                                cmd: cmd_str.clone(),
                                                full_cmd: format!("[thread {}] {}", tid, full_cmd_str),
                                                pss_kb: 0,
                                                uss_kb: 0,
                                                rss_kb: 0,
                                                swap_kb: 0,
                                                state: '?',
                                                cpu_percent: t_cpu,
                                                is_thread: true,
                                                is_kernel: false,
                                                tree_depth: 0,
                                                tree_mask: 0,
                                                is_last_child: false,
                                            });
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // System‑wide info
    let (total, idle) = read_cpu_times_buf(&mut state.file_buf);
    let cpu_percent = if let Some((prev_total, prev_idle)) = state.prev_sys_cpu {
        let d_total = total.saturating_sub(prev_total);
        let d_idle = idle.saturating_sub(prev_idle);
        if d_total > 0 {
            (1.0 - d_idle as f64 / d_total as f64) * 100.0
        } else {
            0.0
        }
    } else {
        0.0
    };
    state.prev_sys_cpu = Some((total, idle));

    let meminfo = read_meminfo_buf(&mut state.file_buf);
    let mem_total = *meminfo.get("MemTotal").unwrap_or(&0);
    let mem_avail = *meminfo.get("MemAvailable").unwrap_or(&{
        meminfo.get("MemFree").unwrap_or(&0)
            + meminfo.get("Buffers").unwrap_or(&0)
            + meminfo.get("Cached").unwrap_or(&0)
    });
    let mem_used = mem_total.saturating_sub(mem_avail);
    let mem_percent = if mem_total > 0 {
        (mem_used as f64 / mem_total as f64) * 100.0
    } else {
        0.0
    };

    let swap_total = *meminfo.get("SwapTotal").unwrap_or(&0);
    let swap_free = *meminfo.get("SwapFree").unwrap_or(&0);
    let swap_used = swap_total.saturating_sub(swap_free);
    let swap_percent = if swap_total > 0 {
        (swap_used as f64 / swap_total as f64) * 100.0
    } else {
        0.0
    };

    let load = {
        let path = b"/proc/loadavg\0";
        let ptr = path.as_ptr() as *const libc::c_char;
        proc_read_str(ptr, &mut state.file_buf)
            .ok()
            .map(|s| s.split_whitespace().take(3).collect::<Vec<_>>().join(" "))
            .unwrap_or_default()
    };
    let uptime = {
        let path = b"/proc/uptime\0";
        let ptr = path.as_ptr() as *const libc::c_char;
        proc_read_str(ptr, &mut state.file_buf)
            .ok()
            .and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok())
            .map(|s| {
                let s = s as u64;
                let y = s / (365 * 86400);
                let r = s % (365 * 86400);
                let d = r / 86400;
                let r = r % 86400;
                let h = r / 3600;
                let r = r % 3600;
                let m = r / 60;
                let sec = r % 60;
                format!("{}y {}d {}h {}m {}s", y, d, h, m, sec)
            })
            .unwrap_or_default()
    };

    let (mut run, mut slp, mut zom) = (0, 0, 0);
    for p in &procs {
        match p.state {
            'R' => run += 1,
            'S' | 'D' => slp += 1,
            'Z' => zom += 1,
            _ => {}
        }
    }
    let tasks = format!(
        "{} ({} run, {} slp, {} zom)",
        procs.iter().filter(|p| !p.is_thread).count(),
        run,
        slp,
        zom
    );

    let sys_info = SystemInfo {
        cpu_percent,
        mem_percent,
        swap_percent,
        load,
        uptime,
        tasks,
    };

    (procs, sys_info)
}

fn worker_loop(
    tx: mpsc::Sender<(Vec<ProcInfo>, SystemInfo)>,
    interval_shared: Arc<AtomicU64>,
    mem_override_shared: Arc<AtomicBool>,
    thread_mode_shared: Arc<AtomicBool>,
    kernel_shared: Arc<AtomicBool>,
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
        path_buf: [0u8; 128],
        file_buf: [0u8; PROC_BUF_SIZE],
    };

    let priming_int = f64::from_bits(interval_shared.load(Ordering::Relaxed));
    let priming_nano = priming_int < 0.5;
    let priming_thread = thread_mode_shared.load(Ordering::Relaxed);
    let priming_kernel = kernel_shared.load(Ordering::Relaxed);
    let _ = gather_data(
        true,
        priming_nano,
        priming_thread,
        priming_kernel,
        &mut state,
        &user_map,
        clk_tck,
        num_cpus,
    );
    thread::sleep(Duration::from_millis(100));

    loop {
        let start = Instant::now();
        let int_sec = f64::from_bits(interval_shared.load(Ordering::Relaxed));
        let thread_mode = thread_mode_shared.load(Ordering::Relaxed);
        let show_kernel = kernel_shared.load(Ordering::Relaxed);

        let use_nano_mode = int_sec < 0.5;

        if state.last_use_nano_mode != use_nano_mode {
            state.prev_cpu_times.clear();
            state.last_use_nano_mode = use_nano_mode;
            let _ = gather_data(
                true,
                use_nano_mode,
                thread_mode,
                show_kernel,
                &mut state,
                &user_map,
                clk_tck,
                num_cpus,
            );
            thread::sleep(Duration::from_millis(100));
        }

        let do_mem = mem_override_shared.load(Ordering::Relaxed)
            || state.last_mem_update.elapsed().as_secs_f64() >= 2.0;
        if do_mem {
            state.last_mem_update = Instant::now();
        }

        let (procs, sys) = gather_data(
            do_mem,
            use_nano_mode,
            thread_mode,
            show_kernel,
            &mut state,
            &user_map,
            clk_tck,
            num_cpus,
        );

        state.last_procs.clear();
        for p in &procs {
            state
                .last_procs
                .insert(if p.is_thread { p.tid } else { p.pid }, p.clone());
        }

        let _ = tx.send((procs, sys));

        let elapsed = start.elapsed();
        let sleep_time = int_sec - elapsed.as_secs_f64();
        let start_mem_ov = mem_override_shared.load(Ordering::Relaxed);
        let start_thread_mode = thread_mode_shared.load(Ordering::Relaxed);
        let start_kernel = kernel_shared.load(Ordering::Relaxed);

        if sleep_time > 0.0 {
            let mut remaining = sleep_time;
            while remaining > 0.0 {
                let chunk = remaining.min(0.05);
                thread::sleep(Duration::from_secs_f64(chunk));

                let new_int = f64::from_bits(interval_shared.load(Ordering::Relaxed));
                let new_mem_ov = mem_override_shared.load(Ordering::Relaxed);
                let new_thread_mode = thread_mode_shared.load(Ordering::Relaxed);
                let new_kernel = kernel_shared.load(Ordering::Relaxed);

                if new_int != int_sec
                    || new_mem_ov != start_mem_ov
                    || new_thread_mode != start_thread_mode
                    || new_kernel != start_kernel
                {
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
            "-i" | "--interval" => {
                i += 1;
                if i < args.len() {
                    interval = args[i].parse().unwrap_or(2.0);
                }
            }
            "--no-color" => no_color = true,
            _ => {}
        }
        i += 1;
    }
    let intervals: [f64; 16] = [
        0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 3.0, 5.0, 10.0, 15.0, 20.0, 30.0, 60.0, 120.0, 180.0, 300.0,
    ];
    if !intervals.contains(&interval) {
        interval = intervals
            .iter()
            .min_by(|a, b| {
                (**a - interval)
                    .abs()
                    .partial_cmp(&(**b - interval).abs())
                    .unwrap()
            })
            .copied()
            .unwrap_or(2.0);
    }

    let interval_shared = Arc::new(AtomicU64::new(interval.to_bits()));
    let mem_override_shared = Arc::new(AtomicBool::new(false));
    let thread_mode_shared = Arc::new(AtomicBool::new(false));
    let kernel_shared = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();

    let user_map = build_user_map();
    let clk_tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as u64;
    let num_cpus = num_cpus::get() as u64;

    thread::spawn({
        let interval_shared = Arc::clone(&interval_shared);
        let mem_override_shared = Arc::clone(&mem_override_shared);
        let thread_mode_shared = Arc::clone(&thread_mode_shared);
        let kernel_shared = Arc::clone(&kernel_shared);
        let user_map = user_map.clone();
        move || {
            worker_loop(
                tx,
                interval_shared,
                mem_override_shared,
                thread_mode_shared,
                kernel_shared,
                user_map,
                clk_tck,
                num_cpus,
            );
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
                if let Event::Key(k) = event::read()? {
                    keys.push(k);
                }
                if !event::poll(Duration::ZERO)? {
                    break;
                }
            }
            for key in keys {
                if app.kill_confirming {
                    match key.code {
                        KeyCode::Char('y') | KeyCode::Char('Y') => {
                            for &pid in &app.marked_pids {
                                unsafe {
                                    libc::kill(pid, libc::SIGTERM);
                                }
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
                            app.mark_and_advance();
                            ui_dirty = true;
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

                if app.searching {
                    match key.code {
                        KeyCode::Esc => {
                            app.filter_string.clear();
                            app.searching = false;
                            ui_dirty = true;
                            continue;
                        }
                        KeyCode::Enter => {
                            app.searching = false;
                            ui_dirty = true;
                            continue;
                        }
                        KeyCode::Backspace | KeyCode::Delete => {
                            app.filter_string.pop();
                            ui_dirty = true;
                            continue;
                        }
                        KeyCode::Char(c) if c.is_ascii_graphic() || c == ' ' => {
                            app.filter_string.push(c);
                            ui_dirty = true;
                            continue;
                        }
                        _ => continue,
                    }
                }

                match key.code {
                    KeyCode::Char('q') | KeyCode::Char('Q') => break 'outer,
                    KeyCode::Char('/') => {
                        app.searching = true;
                        ui_dirty = true;
                        continue;
                    }
                    KeyCode::F(9) => {
                        if !app.filtered_raw.is_empty() {
                            app.kill_mode = true;
                            app.kill_selected = 0;
                            app.marked_pids.clear();
                            app.kill_confirming = false;
                            ui_dirty = true;
                        }
                        continue;
                    }
                    KeyCode::Char('k') => {
                        app.show_kernel = !app.show_kernel;
                        kernel_shared.store(app.show_kernel, Ordering::Relaxed);
                        ui_dirty = true;
                        continue;
                    }
                    KeyCode::Char('t') | KeyCode::Char('T') => {
                        app.tree_mode = !app.tree_mode;
                        if app.tree_mode {
                            app.collapsed_pids.clear();
                        }
                        ui_dirty = true;
                        continue;
                    }
                    KeyCode::Char('h') | KeyCode::Char('H') => {
                        app.thread_mode = !app.thread_mode;
                        thread_mode_shared.store(app.thread_mode, Ordering::Relaxed);
                        ui_dirty = true;
                        continue;
                    }
                    KeyCode::Char('m') | KeyCode::Char('M') => {
                        app.mem_override = !app.mem_override;
                        mem_override_shared.store(app.mem_override, Ordering::Relaxed);
                        ui_dirty = true;
                    }
                    KeyCode::Esc => {
                        app.filter_string.clear();
                        ui_dirty = true;
                        continue;
                    }
                    KeyCode::Up => {
                        app.scroll_pos = app.scroll_pos.saturating_sub(5);
                        ui_dirty = true;
                    }
                    KeyCode::Down => {
                        let vis = app.term_height.saturating_sub(3);
                        let max = if app.filtered_raw.len() > vis {
                            app.filtered_raw.len() - vis
                        } else {
                            0
                        };
                        app.scroll_pos = (app.scroll_pos + 5).min(max);
                        ui_dirty = true;
                    }
                    KeyCode::Left => {
                        app.cmd_offset = app.cmd_offset.saturating_sub(40);
                        app.apply_filter_and_sort();
                        app.compute_widths();
                        ui_dirty = true;
                        continue;
                    }
                    KeyCode::Right => {
                        app.cmd_offset = (app.cmd_offset + 40).min(2000);
                        app.apply_filter_and_sort();
                        app.compute_widths();
                        ui_dirty = true;
                        continue;
                    }
                    KeyCode::Char('+') | KeyCode::Char('=') => {
                        if let Some(pos) = intervals.iter().position(|&v| v == app.interval) {
                            if pos < intervals.len() - 1 {
                                app.interval = intervals[pos + 1];
                                interval_shared.store(app.interval.to_bits(), Ordering::Relaxed);
                                ui_dirty = true;
                            }
                        }
                    }
                    KeyCode::Char('-') => {
                        if let Some(pos) = intervals.iter().position(|&v| v == app.interval) {
                            if pos > 0 {
                                app.interval = intervals[pos - 1];
                                interval_shared.store(app.interval.to_bits(), Ordering::Relaxed);
                                ui_dirty = true;
                            }
                        }
                    }
                    KeyCode::Char(c) if c.is_ascii_digit() => {
                        let n = c.to_digit(10).unwrap() as usize;
                        if (1..=8).contains(&n) {
                            let col = n - 1;
                            if col == app.sort_col {
                                app.sort_reverse = !app.sort_reverse;
                            } else {
                                app.sort_col = col;
                                app.sort_reverse = col > 2;
                            }
                            ui_dirty = true;
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

        let render_interval = if app.searching || app.kill_mode || app.kill_confirming {
            0.1
        } else {
            app.interval
        };
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
            let max = if app.filtered_raw.len() > vis {
                app.filtered_raw.len() - vis
            } else {
                0
            };
            app.scroll_pos = app.scroll_pos.min(max);
            if app.searching {
                app.scroll_pos = 0;
            }

            app.render()?;
            last_render = Instant::now();
            ui_dirty = false;
        }

        if terminal::size()
            .map(|(w, h)| w as usize != app.term_width || h as usize != app.term_height)
            .unwrap_or(false)
        {
            let (w, h) = terminal::size().unwrap_or((80, 24));
            app.term_width = w as usize;
            app.term_height = h as usize;
            app.compute_widths();
            app.render()?;
            last_render = Instant::now();
            ui_dirty = false;
        }
    }
    Ok(())
}
