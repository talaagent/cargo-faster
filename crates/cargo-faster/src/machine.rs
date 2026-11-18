//! The machine: load, other builds, memory, disk, and whether the target
//! directory and the cache can share data cheaply.

use crate::context::{Context, probe};
use crate::finding::{Finding, Section, Severity};
use crate::fmt::bytes;
use alloc::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A filesystem a build directory lives on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    /// What lives here (`target dir`, `cache`, `tmp`).
    pub role: String,
    /// The directory.
    pub path: PathBuf,
    /// Filesystem type (`btrfs`, `ext4`, `tmpfs`, ...).
    pub fstype: String,
    /// Mount point.
    pub mount: String,
    /// Device id (equal ids share a filesystem).
    pub dev: u64,
    /// Total bytes.
    pub total: u64,
    /// Available bytes.
    pub avail: u64,
}

/// What we measured.
#[derive(Debug, Clone, Default)]
pub struct Inputs {
    /// CPUs.
    pub cpus: usize,
    /// 1-minute load average.
    pub load1: f64,
    /// 5-minute load average.
    pub load5: f64,
    /// Total memory in bytes.
    pub mem_total: u64,
    /// Available memory in bytes.
    pub mem_avail: u64,
    /// Swap in use in bytes.
    pub swap_used: u64,
    /// Other `rustc` processes running now.
    pub rustc_procs: usize,
    /// Other `cargo` processes running now.
    pub cargo_procs: usize,
    /// The processes using the most CPU while the machine was busy, by
    /// name: (name, CPUs used), most first. Measured on Linux only, and
    /// only when the load is high.
    pub top_cpu: Vec<(String, f64)>,
    /// Filesystems of the build directories.
    pub mounts: Vec<Mount>,
    /// A jobserver cargo would share with other processes: its
    /// `--jobserver-auth` value and the variable that names it.
    pub jobserver: Option<(String, &'static str)>,
    /// What couldn't be measured on this platform (`load`, `memory`,
    /// `other builds`, `filesystems`).
    pub unavailable: Vec<&'static str>,
}

/// Parse `/proc/loadavg`, or macOS's `sysctl -n vm.loadavg`
/// (`{ 1.20 1.31 1.40 }`).
#[must_use]
pub fn parse_loadavg(s: &str) -> (f64, f64) {
    let mut it = s
        .split_whitespace()
        .filter(|x| *x != "{")
        .map(|x| x.parse().unwrap_or(0.0));
    (it.next().unwrap_or(0.0), it.next().unwrap_or(0.0))
}

/// Available bytes from macOS's `vm_stat`: free, inactive and speculative
/// pages, which the kernel hands out without swapping.
#[must_use]
pub fn parse_vm_stat(s: &str) -> Option<u64> {
    let page: u64 = s
        .lines()
        .next()?
        .split("page size of ")
        .nth(1)?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    let pages = |k: &str| {
        s.lines()
            .find_map(|l| l.strip_prefix(k))
            .and_then(|v| v.trim().trim_end_matches('.').parse::<u64>().ok())
            .unwrap_or(0)
    };
    Some((pages("Pages free:") + pages("Pages inactive:") + pages("Pages speculative:")) * page)
}

/// Swap in use from macOS's `sysctl -n vm.swapusage`
/// (`total = 2048.00M  used = 1024.50M  free = ...`).
#[must_use]
pub fn parse_swapusage(s: &str) -> u64 {
    let used = s
        .split("used = ")
        .nth(1)
        .and_then(|r| r.split_whitespace().next())
        .unwrap_or("0");
    let (n, unit) = used.split_at(used.len().saturating_sub(1));
    let mult: f64 = match unit {
        "K" => 1024.0,
        "M" => 1024.0 * 1024.0,
        "G" => 1024.0 * 1024.0 * 1024.0,
        _ => return 0,
    };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let b = (n.parse::<f64>().unwrap_or(0.0) * mult) as u64;
    b
}

/// The filesystem type of the mount at `mount`, from `/proc/self/mounts`
/// (`dev /mnt type opts 0 0`) or macOS's `mount` (`dev on /mnt (apfs, local)`).
#[must_use]
pub fn fstype_from_mounts(mounts: &str, mount: &str) -> Option<String> {
    mounts
        .lines()
        .find_map(|l| {
            if let Some((_, rest)) = l.split_once(" on ") {
                let (point, opts) = rest.rsplit_once(" (")?;
                return (point == mount).then(|| {
                    opts.split([',', ')'])
                        .next()
                        .unwrap_or("")
                        .trim()
                        .to_owned()
                });
            }
            let cols: Vec<&str> = l.split_whitespace().collect();
            (cols.len() >= 3 && cols[1] == mount).then(|| cols[2].to_owned())
        })
        .filter(|t| !t.is_empty())
}

/// A process's name and CPU time (user + system, in clock ticks) from
/// `/proc/PID/stat` (`123 (comm) S ppid ... utime stime ...`). The name
/// may hold spaces and parentheses, so it ends at the last `)`.
#[must_use]
pub fn parse_proc_stat(s: &str) -> Option<(String, u64)> {
    let (head, rest) = s.rsplit_once(')')?;
    let name = head.split_once('(')?.1;
    let cols: Vec<&str> = rest.split_whitespace().collect();
    let utime: u64 = cols.get(11)?.parse().ok()?;
    let stime: u64 = cols.get(12)?.parse().ok()?;
    Some((name.to_owned(), utime + stime))
}

/// How many processes [`top_cpu`] names.
const TOP_CPU: usize = 5;

/// The process names that used the most CPU between two samples of
/// pid -> (name, ticks), as CPUs (1.0 = one core busy), most first, at
/// least a tenth of a CPU each. Processes are grouped by name, so 30
/// `rustc`s show up as one line, and kernel threads by their kind
/// (`kworker/u130:20-events_unbound` is a `kworker`, and renames itself as
/// it picks up work); a process that started in between counts from zero.
#[must_use]
pub fn top_cpu(
    before: &BTreeMap<u32, (String, u64)>,
    after: &BTreeMap<u32, (String, u64)>,
    ticks: f64,
) -> Vec<(String, f64)> {
    let mut by_name: BTreeMap<&str, u64> = BTreeMap::new();
    for (pid, (name, t)) in after {
        let t0 = before.get(pid).map_or(0, |b| b.1);
        let name = name.split('/').next().unwrap_or(name);
        *by_name.entry(name).or_default() += t.saturating_sub(t0);
    }
    #[allow(clippy::cast_precision_loss)]
    let mut v: Vec<(String, f64)> = by_name
        .into_iter()
        .map(|(n, d)| (n.to_owned(), d as f64 / ticks.max(1.0)))
        .filter(|(_, c)| *c >= 0.1)
        .collect();
    v.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v.truncate(TOP_CPU);
    v
}

/// Every process's name and CPU ticks from `/proc`, but ours.
fn proc_cpu() -> Option<BTreeMap<u32, (String, u64)>> {
    let me = std::process::id();
    let mut out = BTreeMap::new();
    for e in std::fs::read_dir("/proc").ok()?.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        if pid == me {
            continue;
        }
        if let Some(st) = read(&e.path().join("stat").to_string_lossy())
            .as_deref()
            .and_then(parse_proc_stat)
        {
            out.insert(pid, st);
        }
    }
    Some(out)
}

/// The top CPU consumers over half a second, from `/proc` (Linux only;
/// empty elsewhere). `/proc` counts in `USER_HZ` ticks, which is 100 on
/// every mainstream Linux.
fn sample_top_cpu() -> Vec<(String, f64)> {
    const SECS: f64 = 0.5;
    let Some(before) = proc_cpu() else {
        return Vec::new();
    };
    std::thread::sleep(core::time::Duration::from_secs_f64(SECS));
    proc_cpu().map_or_else(Vec::new, |after| top_cpu(&before, &after, 100.0 * SECS))
}

/// "nix-daemon 8.0 CPUs, rustc 2.5 CPUs".
fn top_cpu_list(top: &[(String, f64)]) -> String {
    top.iter()
        .map(|(n, c)| format!("{n} {c:.1} CPUs"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Parse `/proc/meminfo` into (total, available, swap used) bytes.
#[must_use]
pub fn parse_meminfo(s: &str) -> (u64, u64, u64) {
    let kb = |k: &str| {
        s.lines()
            .find_map(|l| l.strip_prefix(k))
            .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
            .unwrap_or(0)
            * 1024
    };
    (
        kb("MemTotal:"),
        kb("MemAvailable:"),
        kb("SwapTotal:").saturating_sub(kb("SwapFree:")),
    )
}

/// Parse `df -PT DIR` (GNU) or `df -P DIR` (macOS and the BSDs, where
/// `-T` means something else) output: one data line. Without a type
/// column, or when df prints `-` (bind mounts, some sandboxes), the type
/// is `unknown`.
#[must_use]
pub fn parse_df(s: &str) -> Option<(String, u64, u64, String)> {
    let typed = s.lines().next()?.split_whitespace().nth(1) == Some("Type");
    let line = s.lines().nth(1)?;
    let cols: Vec<&str> = line.split_whitespace().collect();
    let (fstype, rest) = if typed {
        (*cols.get(1)?, cols.get(2..)?)
    } else {
        ("-", cols.get(1..)?)
    };
    if rest.len() < 5 {
        return None;
    }
    let fstype = if fstype == "-" { "unknown" } else { fstype };
    Some((
        fstype.to_owned(),
        rest[0].parse::<u64>().ok()? * 1024,
        rest[2].parse::<u64>().ok()? * 1024,
        rest[4..].join(" "),
    ))
}

fn existing_ancestor(p: &Path) -> Option<&Path> {
    p.ancestors().find(|a| a.exists())
}

#[cfg(unix)]
fn dev_of(p: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(p).map_or(0, |m| m.dev())
}

#[cfg(not(unix))]
fn dev_of(_: &Path) -> u64 {
    0
}

fn read(p: &str) -> Option<String> {
    std::fs::read_to_string(p).ok()
}

/// A probe's stdout, if it ran and succeeded.
fn run(prog: &str, args: &[&str]) -> Option<String> {
    probe(prog)
        .args(args)
        .output()
        .ok()
        .filter(bpaf_kit::cmd::Output::success)
        .map(|o| o.stdout_lossy().into_owned())
}

/// (total, available, swap used) bytes: `/proc/meminfo`, else macOS's
/// `sysctl` and `vm_stat`.
fn memory() -> Option<(u64, u64, u64)> {
    if let Some(m) = read("/proc/meminfo") {
        return Some(parse_meminfo(&m));
    }
    let total = run("sysctl", &["-n", "hw.memsize"])?
        .trim()
        .parse::<u64>()
        .ok()?;
    let avail = parse_vm_stat(&run("vm_stat", &[])?)?;
    let swap = run("sysctl", &["-n", "vm.swapusage"]).map_or(0, |s| parse_swapusage(&s));
    Some((total, avail, swap))
}

/// Other (rustc, cargo) processes: `/proc/*/comm`, else `ps`.
fn processes() -> Option<(usize, usize)> {
    let me = std::process::id();
    let (mut rustc, mut cargo) = (0_usize, 0_usize);
    let mut count = |pid: Option<u32>, comm: &str| {
        if pid == Some(me) {
            return;
        }
        match comm.trim().rsplit('/').next().unwrap_or(comm) {
            "rustc" => rustc += 1,
            "cargo" => cargo += 1,
            _ => {}
        }
    };
    if Path::new("/proc/self/comm").exists() {
        for e in std::fs::read_dir("/proc").ok()?.flatten() {
            let pid = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok());
            if pid.is_some() {
                count(
                    pid,
                    &read(&e.path().join("comm").to_string_lossy()).unwrap_or_default(),
                );
            }
        }
    } else {
        for l in run("ps", &["-A", "-o", "pid=,comm="])?.lines() {
            let (pid, comm) = l.trim().split_once(' ').unwrap_or(("", ""));
            count(pid.parse().ok(), comm);
        }
    }
    Some((rustc, cargo))
}

/// The filesystem each directory is on. GNU df takes -T for the type
/// column; on macOS and the BSDs -T filters by type, so they get plain -P
/// and the type comes from the mount table, as it does when df prints `-`.
fn mounts(dirs: Vec<(String, PathBuf)>) -> Vec<Mount> {
    let gnu = cfg!(target_os = "linux");
    let table = read("/proc/self/mounts").or_else(|| run("mount", &[]));
    dirs.into_iter()
        .filter_map(|(role, path)| {
            let dir = existing_ancestor(&path)?.to_path_buf();
            let df = |flags: &str| {
                let out = probe("df").arg(flags).arg(&dir).output().ok()?;
                parse_df(&out.stdout_lossy())
            };
            let (mut fstype, total, avail, mount) = if gnu {
                df("-PT").or_else(|| df("-P"))?
            } else {
                df("-P")?
            };
            if fstype == "unknown"
                && let Some(t) = table.as_deref().and_then(|t| fstype_from_mounts(t, &mount))
            {
                fstype = t;
            }
            // A bind mount (a sandbox's /tmp) may be in no mount table;
            // GNU stat asks the kernel.
            if fstype == "unknown"
                && gnu
                && let Some(t) = run("stat", &["-f", "-c", "%T", &dir.to_string_lossy()])
                && !t.trim().is_empty()
            {
                t.trim().clone_into(&mut fstype);
            }
            Some(Mount {
                role,
                dev: dev_of(&dir),
                path,
                fstype,
                mount,
                total,
                avail,
            })
        })
        .collect()
}

impl Inputs {
    /// Measure the real machine: from `/proc` on Linux, from `sysctl`,
    /// `vm_stat`, `ps` and `mount` on macOS. What it can't measure is
    /// listed in [`Inputs::unavailable`].
    #[must_use]
    pub fn gather(ctx: &Context, cache_dir: Option<PathBuf>) -> Self {
        let mut unavailable = Vec::new();
        let load = read("/proc/loadavg").or_else(|| run("sysctl", &["-n", "vm.loadavg"]));
        if load.is_none() {
            unavailable.push("load");
        }
        let (load1, load5) = parse_loadavg(load.as_deref().unwrap_or_default());
        let (mem_total, mem_avail, swap_used) = memory().unwrap_or_else(|| {
            unavailable.push("memory");
            (0, 0, 0)
        });
        let (rustc_procs, mut cargo_procs) = processes().unwrap_or_else(|| {
            unavailable.push("other builds");
            (0, 0)
        });
        // We are run by one cargo ourselves.
        if ctx.var("CARGO").is_some() {
            cargo_procs = cargo_procs.saturating_sub(1);
        }
        // Naming what keeps the machine busy costs half a second, so only
        // when it is.
        #[allow(clippy::cast_precision_loss)]
        let top_cpu = if load1 >= ctx.cpus().max(1) as f64 * 0.5 {
            sample_top_cpu()
        } else {
            Vec::new()
        };
        let mut dirs = vec![("target dir".to_owned(), ctx.target_dir())];
        if let Some(c) = cache_dir {
            dirs.push(("cache".to_owned(), c));
        }
        dirs.push(("tmp".to_owned(), std::env::temp_dir()));
        let ndirs = dirs.len();
        let mounts = mounts(dirs);
        if mounts.len() < ndirs {
            unavailable.push("filesystems");
        }
        Self {
            cpus: ctx.cpus(),
            load1,
            load5,
            mem_total,
            mem_avail,
            swap_used,
            rustc_procs,
            cargo_procs,
            top_cpu,
            mounts,
            jobserver: ctx.jobserver().map(|(var, auth)| (auth.to_owned(), var)),
            unavailable,
        }
    }
}

/// Where a cache keeps its artifacts, guessed from the wrapper.
#[must_use]
pub fn cache_dir(ctx: &Context) -> Option<PathBuf> {
    let w = crate::cache::wrapper(ctx)?.0;
    let home = PathBuf::from(ctx.var("HOME")?);
    if w.contains("kache") {
        return Some(
            ctx.var("KACHE_CACHE_DIR")
                .map_or_else(|| home.join(".cache/kache-store"), PathBuf::from),
        );
    }
    if w.contains("sccache") {
        return Some(ctx.var("SCCACHE_DIR").map_or_else(
            || sccache_default_dir(&home, ctx.var("XDG_CACHE_HOME"), cfg!(target_os = "macos")),
            PathBuf::from,
        ));
    }
    None
}

/// sccache's local cache without `SCCACHE_DIR`: the platform's cache
/// directory plus its app name.
#[must_use]
pub fn sccache_default_dir(home: &Path, xdg_cache: Option<&str>, macos: bool) -> PathBuf {
    if macos {
        return home.join("Library/Caches/Mozilla.sccache");
    }
    xdg_cache
        .filter(|x| !x.is_empty())
        .map_or_else(|| home.join(".cache"), PathBuf::from)
        .join("sccache")
}

const REFLINK_FS: &[&str] = &["btrfs", "xfs", "bcachefs", "apfs", "zfs"];

/// The load, other builds, memory and what couldn't be measured.
fn facts(m: &Inputs, s: &mut Section) {
    #[allow(clippy::cast_precision_loss)]
    let cpus = m.cpus.max(1) as f64;
    let has = |what: &str| !m.unavailable.contains(&what);
    s.fact("cpus", m.cpus.to_string());
    if has("load") {
        s.fact(
            "load",
            format!(
                "{:.1} (1 min), {:.1} (5 min) = {:.0}% of the CPUs",
                m.load1,
                m.load5,
                100.0 * m.load1 / cpus
            ),
        );
    }
    if has("other builds") {
        s.fact(
            "other builds",
            format!("{} rustc, {} cargo processes", m.rustc_procs, m.cargo_procs),
        );
    }
    if !m.top_cpu.is_empty() {
        s.fact("top cpu", top_cpu_list(&m.top_cpu));
    }
    if let Some((auth, var)) = &m.jobserver {
        s.fact(
            "jobserver",
            format!(
                "{auth} ({var}): cargo runs a unit per token it gets, shared with every process on that jobserver"
            ),
        );
    }
    if !m.unavailable.is_empty() {
        s.fact(
            "unavailable",
            format!(
                "{} (not measurable on this platform; cargo faster is Linux-first)",
                m.unavailable.join(", ")
            ),
        );
    }
    if m.mem_total > 0 {
        s.fact(
            "memory",
            format!("{} available of {}", bytes(m.mem_avail), bytes(m.mem_total)),
        );
    }
}

/// Run the check.
#[must_use]
pub fn check(m: &Inputs) -> Section {
    let mut s = Section::new("machine");
    let mut f = Vec::new();
    #[allow(clippy::cast_precision_loss)]
    let cpus = m.cpus.max(1) as f64;
    let has = |what: &str| !m.unavailable.contains(&what);
    facts(m, &mut s);
    if (has("load") || has("other builds")) && (m.load1 >= cpus * 0.5 || m.rustc_procs >= 4) {
        let sev = if m.load1 >= cpus {
            Severity::High
        } else {
            Severity::Medium
        };
        let top = if m.top_cpu.is_empty() {
            String::new()
        } else {
            format!(" Using the most CPU: {}.", top_cpu_list(&m.top_cpu))
        };
        // A condition of the moment, not a fix for the project.
        s.conditions.push(
            Finding::new("busy-machine", sev, "The machine is busy: timings will be noisy and slower", "the-machine")
                .saw(format!("Load average {:.1} on {} CPUs, with {} rustc and {} cargo processes running besides this one.{top}", m.load1, m.cpus, m.rustc_procs, m.cargo_procs))
                .why("Builds compete for cores, memory bandwidth and disk. A build that takes 60 s on an idle machine can take twice that next to two others, so one-off timings say more about the neighbours than about your change.")
                .fix("Measure on a quiet machine, or compare before and after back to back under similar load. Run long experiments under `nice -n 10` on shared boxes. If several agents build at once, a shared cache helps more than more jobs."),
        );
    }
    let short = m.mem_avail < m.mem_total / 10;
    let tight = m.mem_avail < m.mem_total / 4 && m.swap_used > 1 << 30;
    if m.mem_total > 0 && (short || tight) {
        f.push(
            Finding::new("low-memory", if short { Severity::High } else { Severity::Medium }, "Memory is short", "the-machine")
                .saw(format!("{} available of {}, {} of swap in use.", bytes(m.mem_avail), bytes(m.mem_total), bytes(m.swap_used)))
                .why("rustc on a big crate can use several GB, and linking big test binaries more. Swapping makes everything crawl, and the OOM killer turns that into a mysterious failure.")
                .fix("Lower parallelism (`CARGO_BUILD_JOBS`), use `debug = \"line-tables-only\"` to shrink link inputs, or stop other builds."),
        );
    }
    let mut warned: Vec<&str> = Vec::new();
    for mt in &m.mounts {
        let used = 100.0 - crate::fmt::pct(mt.avail as f64, mt.total as f64);
        s.fact(
            format!("{} fs", mt.role),
            format!(
                "{} on {} ({}), {} free",
                mt.path.display(),
                mt.mount,
                mt.fstype,
                bytes(mt.avail)
            ),
        );
        if mt.role != "tmp"
            && (used >= 90.0 || mt.avail < 20 << 30)
            && !warned.contains(&mt.mount.as_str())
        {
            warned.push(&mt.mount);
            f.push(
                Finding::new("disk-full", if used >= 95.0 { Severity::High } else { Severity::Medium }, format!("The {} filesystem is {used:.0}% full", mt.role), "the-machine")
                    .saw(format!("{} has {} free of {}.", mt.mount, bytes(mt.avail), bytes(mt.total)))
                    .why("Target directories grow fast (tens of GB per worktree). A nearly full disk makes caches evict, and copy-on-write filesystems get slow when nearly full.")
                    .fix("Clean stale target directories (`cargo clean` in old worktrees, or your cache's cleanup command) and keep at least 15% free."),
            );
        }
    }
    let target = m.mounts.iter().find(|x| x.role == "target dir");
    let cache = m.mounts.iter().find(|x| x.role == "cache");
    if let (Some(t), Some(c)) = (target, cache) {
        if t.dev != c.dev && t.dev != 0 && c.dev != 0 {
            f.push(
                Finding::new("cache-other-filesystem", Severity::Medium, "The cache and the target directory are on different filesystems", "worktrees-and-cold-target-directories")
                    .saw(format!("target is on {} ({}), the cache on {} ({}).", t.mount, t.fstype, c.mount, c.fstype))
                    .why("A cache hit can only be a reflink or hard link within one filesystem. Across filesystems every hit copies the artifact.")
                    .fix("Move the cache store onto the same filesystem as your checkouts."),
            );
        } else if !REFLINK_FS.contains(&t.fstype.as_str()) {
            f.push(
                Finding::new("no-reflinks", Severity::Low, format!("{} can't reflink", t.fstype), "worktrees-and-cold-target-directories")
                    .saw(format!("target and cache share {} ({}), which has no copy-on-write clones.", t.mount, t.fstype))
                    .why("Caches restore artifacts by copying (or hard-linking, which is fragile) on filesystems without reflinks.")
                    .fix("Nothing to do quickly; btrfs or XFS make cache hits nearly free."),
            );
        }
    }
    s.extend(f);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mount(role: &str, dev: u64, fstype: &str, avail_gb: u64) -> Mount {
        Mount {
            role: role.into(),
            path: "/x".into(),
            fstype: fstype.into(),
            mount: format!("/m{dev}"),
            dev,
            total: 1000 << 30,
            avail: avail_gb << 30,
        }
    }

    #[test]
    fn parsers() {
        assert_eq!(parse_loadavg("14.20 18.03 20.11 9/2000 123"), (14.2, 18.03));
        assert_eq!(parse_loadavg("{ 2.50 1.75 1.60 }\n"), (2.5, 1.75));
        let (t, a, s) = parse_meminfo(
            "MemTotal:       1024 kB\nMemAvailable:    512 kB\nSwapTotal: 100 kB\nSwapFree: 40 kB\n",
        );
        assert_eq!((t, a, s), (1 << 20, 1 << 19, 60 * 1024));
        let df = "Filesystem     Type  1024-blocks      Used Available Capacity Mounted on\n/dev/nvme0n1p2 btrfs  3772782592 3464338596 308443996      92% /home\n";
        assert_eq!(
            parse_df(df).unwrap(),
            (
                "btrfs".into(),
                3_772_782_592 * 1024,
                308_443_996 * 1024,
                "/home".into()
            )
        );
    }

    #[test]
    fn macos_and_sandbox_parsers() {
        // macOS `df -P`: no type column; a mount point with a space.
        let df = "Filesystem     1024-blocks      Used Available Capacity  Mounted on\n/dev/disk3s5    971350180 600000000 300000000    67%    /System/Volumes/Data\n";
        assert_eq!(
            parse_df(df).unwrap(),
            (
                "unknown".into(),
                971_350_180 * 1024,
                300_000_000 * 1024,
                "/System/Volumes/Data".into()
            )
        );
        // GNU df in a sandbox prints `-` for the type: the mount table has it.
        let df = "Filesystem Type 1024-blocks Used Available Capacity Mounted on\ntmpfs - 1000 10 990 1% /tmp\n";
        assert_eq!(parse_df(df).unwrap().0, "unknown");
        let proc_mounts = "/dev/nvme0n1p2 / btrfs rw 0 0\ntmpfs /tmp tmpfs rw,nosuid 0 0\n";
        assert_eq!(
            fstype_from_mounts(proc_mounts, "/tmp").as_deref(),
            Some("tmpfs")
        );
        assert_eq!(fstype_from_mounts(proc_mounts, "/home"), None);
        let mac = "/dev/disk3s1s1 on / (apfs, sealed, local, read-only, journaled)\n/dev/disk3s5 on /System/Volumes/Data (apfs, local, journaled, nobrowse)\n";
        assert_eq!(
            fstype_from_mounts(mac, "/System/Volumes/Data").as_deref(),
            Some("apfs")
        );
        let vm = "Mach Virtual Memory Statistics: (page size of 16384 bytes)\nPages free:                               10000.\nPages active:                             50000.\nPages inactive:                           20000.\nPages speculative:                         1000.\n";
        assert_eq!(parse_vm_stat(vm), Some(31_000 * 16384));
        assert_eq!(
            parse_swapusage("total = 2048.00M  used = 1024.00M  free = 1024.00M  (encrypted)"),
            1 << 30
        );
        assert_eq!(
            parse_swapusage("total = 0.00M  used = 0.00M  free = 0.00M"),
            0
        );
        let home = Path::new("/Users/u");
        assert_eq!(
            sccache_default_dir(home, None, true),
            Path::new("/Users/u/Library/Caches/Mozilla.sccache")
        );
        assert_eq!(
            sccache_default_dir(Path::new("/home/u"), None, false),
            Path::new("/home/u/.cache/sccache")
        );
        assert_eq!(
            sccache_default_dir(Path::new("/home/u"), Some("/xdg"), false),
            Path::new("/xdg/sccache")
        );
    }

    #[test]
    fn says_what_it_cant_measure() {
        let m = Inputs {
            cpus: 8,
            rustc_procs: 9,
            unavailable: vec!["load", "memory", "other builds"],
            ..Inputs::default()
        };
        let s = check(&m);
        assert_eq!(s.findings.len(), 0);
        assert!(
            s.facts
                .iter()
                .all(|f| f.key != "load" && f.key != "jobserver")
        );
        let shared = Inputs {
            jobserver: Some(("fifo:/dev/js".into(), "MAKEFLAGS")),
            ..m.clone()
        };
        assert!(
            check(&shared)
                .facts
                .iter()
                .any(|f| f.key == "jobserver" && f.value.starts_with("fifo:/dev/js (MAKEFLAGS)"))
        );
        let f = s.facts.iter().find(|f| f.key == "unavailable").unwrap();
        assert!(
            f.value
                .starts_with("load, memory, other builds (not measurable on this platform")
        );
    }

    #[test]
    fn busy_full_machine() {
        let m = Inputs {
            cpus: 32,
            load1: 26.0,
            load5: 20.0,
            mem_total: 249 << 30,
            mem_avail: 120 << 30,
            rustc_procs: 7,
            mounts: vec![
                mount("target dir", 1, "btrfs", 80),
                mount("cache", 1, "btrfs", 80),
                mount("tmp", 2, "tmpfs", 10),
            ],
            ..Inputs::default()
        };
        let s = check(&m);
        let ids: Vec<_> = s.findings.iter().map(|f| f.id).collect();
        assert_eq!(ids, ["disk-full"]);
        // Busy is a condition of the moment, not a finding.
        let ids: Vec<_> = s.conditions.iter().map(|f| f.id).collect();
        assert_eq!(ids, ["busy-machine"]);
    }

    #[test]
    fn names_the_top_cpu_consumers() {
        assert_eq!(
            parse_proc_stat(
                "4242 (nix-daemon) R 1 4242 4242 0 -1 4194560 100 0 0 0 750 250 0 0 20 0 1 0 100 0 0"
            ),
            Some(("nix-daemon".into(), 1000))
        );
        // Names may hold spaces and parentheses.
        assert_eq!(
            parse_proc_stat("7 (tokio (rt) w) S 1 7 7 0 -1 0 0 0 0 0 5 6 0 0 20 0 1 0 1 0 0")
                .unwrap(),
            ("tokio (rt) w".into(), 11)
        );
        assert_eq!(parse_proc_stat("garbage"), None);
        let snap = |v: &[(u32, &str, u64)]| -> BTreeMap<u32, (String, u64)> {
            v.iter().map(|&(p, n, t)| (p, (n.to_owned(), t))).collect()
        };
        let before = snap(&[
            (1, "nix-daemon", 1000),
            (2, "rustc", 10),
            (3, "rustc", 20),
            (4, "bash", 5),
            (6, "kworker/u130:20-events_unbound", 9000),
            (7, "kworker/1:0", 9000),
        ]);
        // 50 ticks = half a second: nix-daemon used 8 CPUs, the two rustcs
        // 1 together, a new cc1plus 2, bash nothing, and two kworkers that
        // renamed themselves 0.5 together.
        let after = snap(&[
            (1, "nix-daemon", 1400),
            (2, "rustc", 10),
            (8, "cc1plus", 100),
            (3, "rustc", 70),
            (4, "bash", 5),
            (5, "rustc", 0),
            (6, "kworker/u130:20-btrfs-delayed-meta", 9020),
            (7, "kworker/1:0-mm_percpu_wq", 9005),
        ]);
        let top = top_cpu(&before, &after, 50.0);
        assert_eq!(
            top,
            [
                ("nix-daemon".into(), 8.0),
                ("cc1plus".into(), 2.0),
                ("rustc".into(), 1.0),
                ("kworker".into(), 0.5)
            ]
        );
        let m = Inputs {
            cpus: 32,
            load1: 26.0,
            top_cpu: top,
            ..Inputs::default()
        };
        let s = check(&m);
        assert!(s.conditions[0].saw.ends_with(
            "Using the most CPU: nix-daemon 8.0 CPUs, cc1plus 2.0 CPUs, rustc 1.0 CPUs, kworker 0.5 CPUs."
        ));
        assert!(s.facts.iter().any(|f| f.key == "top cpu"));
    }

    #[test]
    fn cross_filesystem_cache() {
        let m = Inputs {
            cpus: 8,
            mounts: vec![
                mount("target dir", 1, "ext4", 500),
                mount("cache", 2, "ext4", 500),
            ],
            ..Inputs::default()
        };
        let ids: Vec<_> = check(&m).findings.iter().map(|f| f.id).collect();
        assert_eq!(ids, ["cache-other-filesystem"]);
    }
}
