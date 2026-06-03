# jtop – PSS/USS-aware process monitor with true-color TUI

**jtop** is a terminal process viewer focused on two things: 1. Showing the memory your processes actually use. top, htop, even btop and system monitor show Resident Set Size, a calculation for RAM which can greatly overcount (by 20x or more in worst-case) RAM usage by counting shared memory more than once. top/htop/btop/system monitor show RSS. jtop shows **PSS** (Proportional Set Size) and **USS** (Unique Set Size) RAM totals. PSS counts divides shared memory by all processes using it, giving an accurate representation of the amount of RAM a given process actually uses. 2. It measures CPU usage by wall-clock delta with nanosecond‑accurate values from /proc/<pid>/task/<tid>/schedstat for intervals under 0.5 s, preserving accuracy.  
Originally written to compare window‑manager memory footprints, it has grown into a usable `htop` replacement with fast filtering, bulk kill, tree view, thread view, and a readable true‑color interface. It doesn't have the same features as htop yet, but has everything that mattered to me. 

---

## Features

- **Accurate per‑process memory**  
  Reads PSS, USS, RSS, and Swap from `/proc/<pid>/smaps_rollup`.  
  USS is computed as *Private_Clean + Private_Dirty*, giving you the memory that would be freed if the process is killed.  
  *Note:* `/proc/<pid>/smaps_rollup` is only readable by the process owner or root. Running without `sudo` will show memory only for your own processes (others appear with zero memory).

- **True wall‑clock CPU measurement**  
  For intervals **< 0.5 s**, jtop uses per‑task CPU runtime from `/proc/<pid>/task/<tid>/schedstat` (nanosecond granularity).  
  For intervals ≥ 0.5 s, it uses the standard `utime + stime` from `/proc/<pid>/stat`.  
  In both cases, the difference between two samples is divided by the wall‑clock elapsed time **× number of CPUs** – giving a true utilization percentage. No more laggy “jiffy‑based” estimates.

- **Tree view & threads**  
  Toggle tree mode (`t`) to see parent‑child hierarchies with indentation lines.  
  Toggle thread view (`h`) to list individual threads and their CPU usage.  
  Combine both for a full tree of processes and their threads.
  NOTE: sorting is disabled in tree mode, to show parent-child relationships.

- **Mark & kill multiple processes**  
  Press `F9` to enter kill mode. Move with arrow keys, press Space to mark/unmark and advance to the next entry. Pauses output to give you time to select.
  Hold Space to rapidly mark many items. Press Enter to confirm killing all marked PIDs/TIDs (asks for `y/n` before sending `SIGTERM`).

- **Instant filter & search**  
  Press `/` to start typing a filter string; the list updates in real time.  
  Press Esc to clear the filter.  
  The status bar shows the total PSS and swap of all currently visible (filtered) processes. Useful to know, for example, how much RAM all Chrome processes are using. 

- **Customizable update interval**  
  Snap to one of these allowed values: `0.05, 0.1, 0.25, 0.5, 1, 2, 3, 5, 10, 15, 20, 30, 60, 120, 180, 300` seconds.  
  Use `+`/`-` keys to increase/decrease the interval during runtime, or set it at launch with `-i <seconds>`.

- **Memory‑override mode**  
  By default, PSS/USS/RSS/Swap are only refreshed every 2 seconds (CPU always updates every interval).  
  Press `m` to toggle **mem‑override**, which forces a full memory re‑read on every interval – useful for sub‑second monitoring at the cost of significantly higher CPU usage. This is basically unavoidable, as getting PSS RAM usage is much more expensive than RSS, though it may be able to be optimized more in the future for better efficiency. 

- **Kernel process visibility**  
  Press `k` to show kernel threads. They are displayed in a dimmed gray color.

- **Color‑coded columns**  
  - PSS color ranges from green → yellow → red based on size relative to total memory. Will likely be made configurable in the future.  
  - RSS uses an HSV gradient that cycles in a pattern and grays out as RSS approaches total RAM. Since RSS values don't really mean much, the color scheme doesn't either, intentionally.  
  - Swap uses white → pink → red.  
  - CPU uses green → yellow → red.  
  - Users are colored differently: your user is blue, `root` is red, others are indigo.

- **System overview header**  
  Displays total CPU%, memory%, swap% (with bar graphs), load averages, uptime, and task counts (running/sleeping/zombie).

- **True‑color 24‑bit output** (can be disabled with `--no-color`).

---

## Installation

### From source

A recent Rust toolchain is required (install via [rustup](https://rustup.rs/)).

```bash
git clone https://github.com/jcogitation/jtop.git
cd jtop
cargo build --release
sudo cp target/release/jtop /usr/local/bin/   # or any directory in $PATH
```

After cloning, ensure you have the `./jtop/Cargo.toml`, `./jtop/LICENSE.txt`, and `./jtop/src/main.rs` present.

---

## Usage

```
jtop [-i seconds] [--no-color]
```

| Option            | Description                                                                                                                      |
|-------------------|----------------------------------------------------------------------------------------------------------------------------------|
| `-i`, `--interval`| Update interval in seconds. If not one of the allowed values (see above), it snaps to the nearest allowed value. Default: `2.0`. |
| `--no-color`      | Disable all 24‑bit color output (plain text).                                                                                    |
|-------------------|----------------------------------------------------------------------------------------------------------------------------------|

### Interactive keybindings

| Key          | Action                                                                        |
|--------------|-------------------------------------------------------------------------------|
| `q` / `Q`    | Quit                                                                          |
| `↑` / `↓`    | Scroll process list (5 lines at a time/try scroll wheel on mouse)             |
| `←` / `→`    | Scroll the Command column horizontally (40 chars per step)                    |
| `/`          | Enter filter / search mode (type string, Enter to apply)                      |
| `Esc`        | Clear any active filter / exit search mode                                    |
| `+`          | Increase interval to next allowed value                                       |
| `-`          | Decrease interval to previous allowed value                                   |
| `1` … `8`    | Sort by column. Press again to reverse order.                                 |
|              | 1 = PID, 2 = User, 3 = Command, 4 = PSS, 5 = USS, 6 = RSS, 7 = Swap, 8 = CPU% |
| `t`          | Toggle **tree view** (hierarchical, with indentation)                         |
| `h`          | Toggle **thread view** (show individual threads)                              |
| `k`          | Toggle display of kernel threads                                              |
| `m`          | Toggle **mem‑override** (force full memory update every interval)             |
| `F9`         | Enter **kill mode**                                                           |
|              | *Inside kill mode:*                                                           |
| `↑` / `↓`    | Move cursor                                                                   |
| `Space`      | Mark/unmark the highlighted process/thread and advance to the next            |
| `Enter`      | Confirm kill of all marked entries (prompts y/n)                              |
| `Esc`        | Exit kill mode without killing                                                |
|--------------|-------------------------------------------------------------------------------|

*Tip:* while filtering for a specific program (e.g. `brave`), press `F9`, then hold Space to rapidly mark all visible entries. Press Enter, then `y` to kill them all.

---

## How it works

### Memory data

Every `interval` (or every 2 seconds if mem‑override is off), jtop reads `/proc/<pid>/smaps_rollup` for each process. This kernel‑exposed file contains aggregated PSS, RSS, private clean/dirty, and swap values – much faster than parsing the full `smaps` file. USS is calculated as `Private_Clean + Private_Dirty`.

### CPU measurement

jtop takes two consecutive samples of total CPU time for each process/thread:

- **Sub‑0.5 s intervals:** reads nanosecond‑accurate `schedstat` files from `/proc/<pid>/task/<tid>/schedstat`. The sum of all thread runtimes gives the process runtime.
- **0.5 s and above:** reads `utime + stime` (in jiffies) from `/proc/<pid>/stat`, using `sysconf(_SC_CLK_TCK)` to convert to seconds.

The CPU% is then:

```
CPU% = (delta_cpu_time / delta_wall_time / num_cpus) × 100
```

This gives a true, accurate percentage that reacts instantly to load changes.

### Resource usage

Under normal operation (interval ≥ 0.5 s, mem‑override off) jtop’s CPU usage is comparable to `htop`.  
Extremely short intervals (< 0.25 s) and mem‑override will increase CPU load because more file I/O and computation are required.

---

## License

MIT – see `LICENSE.txt`

---

## Acknowledgments

Built with Rust, [`crossterm`](https://crates.io/crates/crossterm), and a deep desire to see real memory numbers.
