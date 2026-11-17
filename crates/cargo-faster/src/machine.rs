//! The machine: load, other builds, memory, disk, and whether the target
//! directory and the cache can share data cheaply.

use crate::context::{Context, probe};
use crate::finding::{Finding, Section, Severity};
use crate::fmt::bytes;
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
    /// Filesystems of the build directories.
    pub mounts: Vec<Mount>,
}

/// Parse `/proc/loadavg`.
#[must_use]
pub fn parse_loadavg(s: &str) -> (f64, f64) {
    let mut it = s.split_whitespace().map(|x| x.parse().unwrap_or(0.0));
    (it.next().unwrap_or(0.0), it.next().unwrap_or(0.0))
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

/// Parse `df -PT DIR` output (one data line).
#[must_use]
pub fn parse_df(s: &str) -> Option<(String, u64, u64, String)> {
    let line = s.lines().nth(1)?;
    let cols: Vec<&str> = line.split_whitespace().collect();
    if cols.len() < 7 {
        return None;
    }
    // df prints `-` when it can't tell (bind mounts, some sandboxes).
    let fstype = if cols[1] == "-" { "unknown" } else { cols[1] };
    Some((
        fstype.to_owned(),
        cols[2].parse::<u64>().ok()? * 1024,
        cols[4].parse::<u64>().ok()? * 1024,
        cols[6].to_owned(),
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

impl Inputs {
    /// Measure the real machine (Linux; elsewhere most fields stay empty).
    #[must_use]
    pub fn gather(ctx: &Context, cache_dir: Option<PathBuf>) -> Self {
        let read = |p: &str| std::fs::read_to_string(p).unwrap_or_default();
        let (load1, load5) = parse_loadavg(&read("/proc/loadavg"));
        let (mem_total, mem_avail, swap_used) = parse_meminfo(&read("/proc/meminfo"));
        let me = std::process::id();
        let (mut rustc_procs, mut cargo_procs) = (0_usize, 0_usize);
        if let Ok(dir) = std::fs::read_dir("/proc") {
            for e in dir.flatten() {
                let name = e.file_name();
                let Some(pid) = name.to_str().and_then(|n| n.parse::<u32>().ok()) else {
                    continue;
                };
                if pid == me {
                    continue;
                }
                match std::fs::read_to_string(e.path().join("comm"))
                    .unwrap_or_default()
                    .trim()
                {
                    "rustc" => rustc_procs += 1,
                    "cargo" => cargo_procs += 1,
                    _ => {}
                }
            }
        }
        // We are run by one cargo ourselves.
        if ctx.var("CARGO").is_some() {
            cargo_procs = cargo_procs.saturating_sub(1);
        }
        let mut dirs = vec![("target dir".to_owned(), ctx.target_dir())];
        if let Some(c) = cache_dir {
            dirs.push(("cache".to_owned(), c));
        }
        dirs.push(("tmp".to_owned(), std::env::temp_dir()));
        let mounts = dirs
            .into_iter()
            .filter_map(|(role, path)| {
                let dir = existing_ancestor(&path)?.to_path_buf();
                let out = probe("df").arg("-PT").arg(&dir).output().ok()?;
                let (fstype, total, avail, mount) = parse_df(&out.stdout_lossy())?;
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
            .collect();
        Self {
            cpus: ctx.cpus(),
            load1,
            load5,
            mem_total,
            mem_avail,
            swap_used,
            rustc_procs,
            cargo_procs,
            mounts,
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
        return Some(
            ctx.var("SCCACHE_DIR")
                .map_or_else(|| home.join(".cache/sccache"), PathBuf::from),
        );
    }
    None
}

const REFLINK_FS: &[&str] = &["btrfs", "xfs", "bcachefs", "apfs", "zfs"];

/// Run the check.
#[must_use]
pub fn check(m: &Inputs) -> Section {
    let mut s = Section::new("machine");
    let mut f = Vec::new();
    #[allow(clippy::cast_precision_loss)]
    let cpus = m.cpus.max(1) as f64;
    s.fact("cpus", m.cpus.to_string());
    s.fact(
        "load",
        format!(
            "{:.1} (1 min), {:.1} (5 min) = {:.0}% of the CPUs",
            m.load1,
            m.load5,
            100.0 * m.load1 / cpus
        ),
    );
    s.fact(
        "other builds",
        format!("{} rustc, {} cargo processes", m.rustc_procs, m.cargo_procs),
    );
    if m.mem_total > 0 {
        s.fact(
            "memory",
            format!("{} available of {}", bytes(m.mem_avail), bytes(m.mem_total)),
        );
    }
    if m.load1 >= cpus * 0.5 || m.rustc_procs >= 4 {
        let sev = if m.load1 >= cpus {
            Severity::High
        } else {
            Severity::Medium
        };
        f.push(
            Finding::new("busy-machine", sev, "The machine is busy: timings will be noisy and slower", "the-machine")
                .saw(format!("Load average {:.1} on {} CPUs, with {} rustc and {} cargo processes running besides this one.", m.load1, m.cpus, m.rustc_procs, m.cargo_procs))
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
        let ids: Vec<_> = check(&m).findings.iter().map(|f| f.id).collect();
        assert_eq!(ids, ["busy-machine", "disk-full"]);
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
