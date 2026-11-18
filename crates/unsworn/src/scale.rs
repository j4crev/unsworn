//! Deciding how many workers a runtime needs.
//!
//! A pool starts with its fewest workers and adds one when the ones it has
//! can't keep up, until there's one for each CPU. Two things say so:
//!
//! - **A call had to wait for a worker.** Every worker was at its
//!   `max_in_flight`, which is how a worker that takes one request at a
//!   time shows it's busy.
//! - **Every worker used most of a core, and there's a CPU to spare.** A
//!   worker that takes many requests at once never makes a call wait, so
//!   its CPU time is read from `/proc` instead. Another worker only helps
//!   if something can run it: when the CPUs are all busy, one more process
//!   is one more to switch between.
//!
//! A worker that is busy all the time has calls queueing behind the one
//! it's working on, so the pool doesn't wait for proof that another would
//! answer more of them. Requests that arrive at a steady rate are answered
//! at that rate with one worker or two, and much sooner with two.
//!
//! A worker goes once the rest could do the work at half their capacity,
//! and that has been so for a while.

use std::time::{Duration, Instant};

/// How often workers' CPU time is read.
pub const SAMPLE: Duration = Duration::from_millis(250);
/// The share of one core at which a worker counts as busy.
const BUSY: f64 = 0.9;
/// How many samples in a row every worker has to be busy for.
const BUSY_SAMPLES: u32 = 2;
/// How much idle CPU, in CPUs, there has to be for one more worker.
const SPARE_CPU: f64 = 0.5;

/// What to do with the pool after a sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Hold,
    /// Add a worker.
    Grow,
    /// Retire a worker: the others have room for its work.
    Shrink,
}

/// What the pool looked like over one [`SAMPLE`].
#[derive(Debug, Clone, Copy, Default)]
pub struct Sample<'a> {
    /// Each worker's CPU use as a share of one core. Missing for a worker
    /// that has only just started.
    pub loads: &'a [Option<f64>],
    /// How many CPUs' worth of idle time there was, if it could be read.
    pub idle_cpus: Option<f64>,
    /// A call had to wait for a worker, which means none is to spare.
    pub waited: bool,
}

/// The state between samples.
#[derive(Debug)]
pub struct Scaler {
    /// How long the pool has to have a worker to spare before one is
    /// retired.
    spare_for: Duration,
    busy_samples: u32,
    spare_since: Option<Instant>,
}

impl Scaler {
    pub fn new(spare_for: Duration) -> Scaler {
        Scaler {
            spare_for,
            busy_samples: 0,
            spare_since: None,
        }
    }

    /// Takes one sample of a pool that keeps between `min` and `max`
    /// workers, and says what to do with it.
    pub fn step(&mut self, sample: Sample, min: usize, max: usize, now: Instant) -> Decision {
        let n = sample.loads.len();
        let known: Vec<f64> = sample.loads.iter().flatten().copied().collect();
        // A worker that has only just started has no load yet, which also
        // keeps the pool from adding a second before the first is counted.
        let all_known = n > 0 && known.len() == n;

        if all_known && known.iter().all(|load| *load >= BUSY) {
            self.busy_samples += 1;
        } else {
            self.busy_samples = 0;
        }
        let room = sample.idle_cpus.is_none_or(|idle| idle >= SPARE_CPU);
        if n < max && room && self.busy_samples >= BUSY_SAMPLES {
            self.busy_samples = 0;
            self.spare_since = None;
            return Decision::Grow;
        }

        // One fewer could do this at half their capacity.
        let spare = all_known
            && n > min
            && !sample.waited
            && known.iter().sum::<f64>() <= 0.5 * (n as f64 - 1.0);
        if !spare {
            self.spare_since = None;
            return Decision::Hold;
        }
        let since = *self.spare_since.get_or_insert(now);
        if now.duration_since(since) >= self.spare_for {
            self.spare_since = None;
            return Decision::Shrink;
        }
        Decision::Hold
    }
}

/// Clock ticks per second in `/proc/<pid>/stat`. Linux reports CPU times
/// to programs in units of 1/100 s on every architecture Unsworn runs on,
/// whatever the kernel's own timer rate.
const TICKS_PER_SECOND: f64 = 100.0;

/// A share of one core, from CPU ticks used over a period.
pub fn load(ticks: u64, over: Duration) -> f64 {
    if over.is_zero() {
        return 0.0;
    }
    ticks as f64 / TICKS_PER_SECOND / over.as_secs_f64()
}

/// The CPU time (user and system) in a `/proc/<pid>/stat` line, in ticks.
fn stat_ticks(stat: &str) -> Option<u64> {
    // The second field is the command in brackets, which may itself hold
    // spaces and brackets, so count fields from the last `)`.
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut fields = rest.split_ascii_whitespace();
    // After the command come state (field 3) ... utime (14), stime (15).
    let utime: u64 = fields.nth(11)?.parse().ok()?;
    let stime: u64 = fields.next()?.parse().ok()?;
    Some(utime + stime)
}

/// The CPUs in a list such as `0-3,8,10-11`, as `/proc` writes them.
fn cpu_list(list: &str) -> Vec<u32> {
    let mut out = Vec::new();
    for part in list.trim().split(',') {
        let (first, last) = part.split_once('-').unwrap_or((part, part));
        if let (Ok(first), Ok(last)) = (first.parse::<u32>(), last.parse::<u32>()) {
            out.extend(first..=last);
        }
    }
    out
}

/// The idle time in `/proc/stat` on the given CPUs, in ticks.
fn stat_idle(stat: &str, cpus: &[u32]) -> u64 {
    let mut total = 0;
    for line in stat.lines() {
        let mut fields = line.split_ascii_whitespace();
        let Some(cpu) = fields
            .next()
            .and_then(|name| name.strip_prefix("cpu"))
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        if cpus.contains(&cpu) {
            // user nice system idle iowait: waiting for a disk is idle too.
            let idle: u64 = fields.nth(3).and_then(|v| v.parse().ok()).unwrap_or(0);
            let iowait: u64 = fields.next().and_then(|v| v.parse().ok()).unwrap_or(0);
            total += idle + iowait;
        }
    }
    total
}

/// The idle time so far on the CPUs this process may run on, in ticks.
/// Workers inherit the same CPUs.
pub fn idle_ticks() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let allowed = status
        .lines()
        .find_map(|line| line.strip_prefix("Cpus_allowed_list:"))?;
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    Some(stat_idle(&stat, &cpu_list(allowed)))
}

/// A process and its descendants.
///
/// A runtime's command runs through `sh -c`. The shell usually replaces
/// itself with the worker, but not for a command such as `cd x && node
/// worker.js`, where the worker is the shell's child.
fn tree(pid: u32) -> Vec<u32> {
    fn walk(pid: u32, depth: usize, out: &mut Vec<u32>) {
        out.push(pid);
        if depth == 0 {
            return;
        }
        // Each thread lists the children it started. A kernel built
        // without this file reports the process alone.
        let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
            return;
        };
        for task in tasks.flatten() {
            let Ok(children) = std::fs::read_to_string(task.path().join("children")) else {
                continue;
            };
            for child in children.split_ascii_whitespace() {
                if let Ok(child) = child.parse() {
                    walk(child, depth - 1, out);
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(pid, 4, &mut out);
    out
}

/// The CPU time a process and its descendants have used, in ticks. `None`
/// when the process is gone.
pub fn tree_ticks(pid: u32) -> Option<u64> {
    let mut total = None;
    for pid in tree(pid) {
        let ticks = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| stat_ticks(&stat));
        if let Some(ticks) = ticks {
            total = Some(total.unwrap_or(0) + ticks);
        }
    }
    total
}

/// The resident memory in a `/proc/<pid>/status`, in kilobytes.
fn status_rss_kb(status: &str) -> Option<u64> {
    let line = status.lines().find_map(|l| l.strip_prefix("VmRSS:"))?;
    line.split_ascii_whitespace().next()?.parse().ok()
}

/// The memory a process and its descendants hold, in kilobytes.
pub fn tree_memory_kb(pid: u32) -> u64 {
    tree(pid)
        .into_iter()
        .filter_map(|pid| std::fs::read_to_string(format!("/proc/{pid}/status")).ok())
        .filter_map(|status| status_rss_kb(&status))
        .sum()
}

/// CPU ticks as seconds.
pub fn seconds(ticks: u64) -> f64 {
    ticks as f64 / TICKS_PER_SECOND
}

/// What a process has used so far.
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct Usage {
    pub cpu_seconds: f64,
    pub memory_kb: u64,
}

/// What this process has used, without its workers.
pub fn own_usage() -> Usage {
    let read = |file: &str| std::fs::read_to_string(format!("/proc/self/{file}")).ok();
    Usage {
        cpu_seconds: read("stat")
            .and_then(|stat| stat_ticks(&stat))
            .map_or(0.0, seconds),
        memory_kb: read("status")
            .and_then(|status| status_rss_kb(&status))
            .unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPARE_FOR: Duration = Duration::from_secs(30);

    impl Default for Scaler {
        fn default() -> Scaler {
            Scaler::new(SPARE_FOR)
        }
    }

    fn of(loads: &[Option<f64>]) -> Sample<'_> {
        Sample {
            loads,
            ..Sample::default()
        }
    }

    /// Steps a pool of one to four workers.
    fn step(s: &mut Scaler, sample: Sample, now: Instant) -> Decision {
        s.step(sample, 1, 4, now)
    }

    #[test]
    fn grows_when_every_worker_is_busy_for_two_samples() {
        let mut s = Scaler::default();
        let now = Instant::now();
        assert_eq!(step(&mut s, of(&[Some(0.95)]), now), Decision::Hold);
        assert_eq!(step(&mut s, of(&[Some(0.92)]), now), Decision::Grow);
    }

    #[test]
    fn one_busy_worker_among_idle_ones_isnt_a_reason() {
        let mut s = Scaler::default();
        let now = Instant::now();
        for _ in 0..10 {
            assert_eq!(
                step(&mut s, of(&[Some(1.0), Some(0.1)]), now),
                Decision::Hold
            );
        }
        // Nor is a worker that is busy for one sample only.
        assert_eq!(step(&mut s, of(&[Some(1.0)]), now), Decision::Hold);
        assert_eq!(step(&mut s, of(&[Some(0.2)]), now), Decision::Hold);
        assert_eq!(step(&mut s, of(&[Some(1.0)]), now), Decision::Hold);
        // Nor one that still has a fifth of its time.
        for _ in 0..10 {
            assert_eq!(step(&mut s, of(&[Some(0.8)]), now), Decision::Hold);
        }
    }

    #[test]
    fn a_busy_worker_isnt_a_reason_when_no_cpu_is_idle() {
        let mut s = Scaler::default();
        let now = Instant::now();
        let busy = [Some(0.95)];
        let sample = |idle| Sample {
            idle_cpus: Some(idle),
            ..of(&busy)
        };
        // Unsworn and the worker fill every CPU between them.
        for _ in 0..10 {
            assert_eq!(step(&mut s, sample(0.1), now), Decision::Hold);
        }
        // With a CPU free, the same worker gets company.
        assert_eq!(step(&mut s, sample(1.5), now), Decision::Grow);
    }

    #[test]
    fn keeps_growing_while_every_worker_stays_busy() {
        let mut s = Scaler::default();
        let now = Instant::now();
        // Work that fills a core for each worker it's given: the pool adds
        // one at a time, up to the most it may run.
        for n in 1..4 {
            let busy = vec![Some(1.0); n];
            assert_eq!(step(&mut s, of(&busy), now), Decision::Hold, "{n}");
            assert_eq!(step(&mut s, of(&busy), now), Decision::Grow, "{n}");
            // The new worker has no load to show in its first sample, and
            // the pool waits for one before it counts the workers as busy.
            let mut starting = busy.clone();
            starting.push(None);
            assert_eq!(step(&mut s, of(&starting), now), Decision::Hold, "{n}");
        }
        let full = [Some(1.0); 4];
        for _ in 0..10 {
            assert_eq!(step(&mut s, of(&full), now), Decision::Hold);
        }
    }

    #[test]
    fn stops_growing_once_the_workers_have_time_to_spare() {
        let mut s = Scaler::default();
        let now = Instant::now();
        let one = [Some(1.0)];
        assert_eq!(step(&mut s, of(&one), now), Decision::Hold);
        assert_eq!(step(&mut s, of(&one), now), Decision::Grow);
        // Two share what one was doing, and neither is busy.
        let two = [Some(0.55), Some(0.55)];
        for _ in 0..10 {
            assert_eq!(step(&mut s, of(&two), now), Decision::Hold);
        }
    }

    #[test]
    fn never_grows_past_the_most_allowed() {
        let mut s = Scaler::default();
        let now = Instant::now();
        let full = [Some(1.0); 4];
        for _ in 0..10 {
            let waited = Sample {
                waited: true,
                ..of(&full)
            };
            assert_eq!(step(&mut s, waited, now), Decision::Hold);
        }
    }

    #[test]
    fn a_call_that_waited_means_none_is_to_spare() {
        // The workers' CPU says one could go: they take one request at a
        // time and spend it waiting. The calls that queued say otherwise.
        let mut s = Scaler::default();
        let start = Instant::now();
        let loads = [Some(0.0); 3];
        assert_eq!(step(&mut s, of(&loads), start), Decision::Hold);
        let waited = Sample {
            waited: true,
            ..of(&loads)
        };
        assert_eq!(step(&mut s, waited, start + SPARE_FOR), Decision::Hold);
        assert_eq!(
            step(&mut s, of(&loads), start + SPARE_FOR + SAMPLE),
            Decision::Hold
        );
    }

    #[test]
    fn shrinks_only_after_a_while_with_one_to_spare() {
        let mut s = Scaler::default();
        let start = Instant::now();
        let loads = [Some(0.2), Some(0.1), Some(0.1)];
        assert_eq!(step(&mut s, of(&loads), start), Decision::Hold);
        let almost = start + SPARE_FOR - Duration::from_secs(1);
        assert_eq!(step(&mut s, of(&loads), almost), Decision::Hold);
        assert_eq!(
            step(&mut s, of(&loads), start + SPARE_FOR),
            Decision::Shrink
        );
        // And then waits as long again for the next one.
        assert_eq!(
            step(&mut s, of(&loads[..2]), start + SPARE_FOR + SAMPLE),
            Decision::Hold
        );

        // A burst in between starts the wait over.
        let mut s = Scaler::default();
        assert_eq!(step(&mut s, of(&loads), start), Decision::Hold);
        let busy = [Some(0.9), Some(0.6), Some(0.3)];
        assert_eq!(step(&mut s, of(&busy), almost), Decision::Hold);
        assert_eq!(step(&mut s, of(&loads), start + SPARE_FOR), Decision::Hold);
    }

    #[test]
    fn keeps_its_fewest_and_what_it_needs() {
        let start = Instant::now();
        let later = start + SPARE_FOR * 2;
        // One idle worker of a pool that keeps one.
        let mut s = Scaler::default();
        assert_eq!(s.step(of(&[Some(0.0)]), 1, 4, start), Decision::Hold);
        assert_eq!(s.step(of(&[Some(0.0)]), 1, 4, later), Decision::Hold);
        // Two idle workers of a pool that keeps two.
        let mut s = Scaler::default();
        assert_eq!(s.step(of(&[Some(0.0); 2]), 2, 4, start), Decision::Hold);
        assert_eq!(s.step(of(&[Some(0.0); 2]), 2, 4, later), Decision::Hold);
        // Three at 40% each: two would be at 60%, over half.
        let mut s = Scaler::default();
        assert_eq!(s.step(of(&[Some(0.4); 3]), 1, 4, start), Decision::Hold);
        assert_eq!(s.step(of(&[Some(0.4); 3]), 1, 4, later), Decision::Hold);
    }

    #[test]
    fn reads_cpu_time_from_a_stat_line() {
        // A command with a space and a bracket in it.
        let line = "4242 (my (odd) cmd) S 1 4242 4242 0 -1 4194304 500 0 0 0 130 25 7 3 20 0 9 0 100 1000 200 18446744073709551615";
        assert_eq!(stat_ticks(line), Some(155));
        assert_eq!(stat_ticks("garbage"), None);
        assert!((load(50, Duration::from_millis(500)) - 1.0).abs() < 1e-9);
        assert_eq!(load(50, Duration::ZERO), 0.0);
    }

    #[test]
    fn reads_idle_time_on_the_cpus_it_may_use() {
        assert_eq!(cpu_list("0-3,8,10-11\n"), [0, 1, 2, 3, 8, 10, 11]);
        assert_eq!(cpu_list("5"), [5]);
        let stat = "cpu  100 0 50 900 20 0 0 0 0 0\n\
                    cpu0 50 0 25 400 10 0 0 0 0 0\n\
                    cpu1 50 0 25 500 10 0 0 0 0 0\n\
                    intr 12345\n";
        assert_eq!(stat_idle(stat, &[0, 1]), 920);
        assert_eq!(stat_idle(stat, &[1]), 510);
        assert_eq!(stat_idle(stat, &[7]), 0);
        assert!(idle_ticks().is_some_and(|t| t > 0));
    }

    #[test]
    fn reads_this_processs_own_cpu_time() {
        let before = tree_ticks(std::process::id()).expect("/proc is readable");
        let mut x = 0u64;
        let until = Instant::now() + Duration::from_millis(60);
        while Instant::now() < until {
            x = x.wrapping_mul(31).wrapping_add(7);
        }
        std::hint::black_box(x);
        let after = tree_ticks(std::process::id()).unwrap();
        assert!(after > before, "{before} -> {after}");
        assert_eq!(tree_ticks(u32::MAX - 1), None);
        // And its memory.
        assert!(tree_memory_kb(std::process::id()) > 1000);
        let own = own_usage();
        assert!(own.cpu_seconds > 0.0 && own.memory_kb > 1000, "{own:?}");
        assert_eq!(
            status_rss_kb("Name:\tx\nVmRSS:\t   12345 kB\nThreads:\t4\n"),
            Some(12345)
        );
    }
}
