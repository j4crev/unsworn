//! Deciding how many workers a runtime needs.
//!
//! A pool starts with its fewest workers and adds one when the ones it has
//! can't keep up. Three things say so:
//!
//! - **A call had to wait for a worker.** Every worker was at its
//!   `max_in_flight`, which is how a worker that takes one request at a
//!   time shows it's busy.
//! - **Every worker used most of a core, and there's a CPU to spare.** A
//!   worker that takes many requests at once never makes a call wait, so
//!   its CPU time is read from `/proc` instead. Another worker only helps
//!   if something can run it: when the CPUs are all busy, one more process
//!   is one more to switch between.
//! - **Calls are piling up** in workers that are busy, which is what more
//!   requests arriving than they can answer looks like.
//!
//! A busy worker isn't always the reason calls take as long as they do.
//! One Bun or Node worker answering a handler that does nothing uses a
//! whole core at 70,000 calls a second, and a second worker next to it
//! answers no more of them: Unsworn's side of the call is the limit, and
//! each call costs a tenth more CPU with two. So a worker added for the
//! second reason is on trial. If the pool doesn't answer at least a tenth
//! more calls with it, it's retired again, and the pool waits a while,
//! longer each time, before it tries another.
//!
//! A worker also goes once the rest could do the work at half their
//! capacity, and that has been so for a while.

use std::time::{Duration, Instant};

/// How often workers' CPU time is read.
pub const SAMPLE: Duration = Duration::from_millis(250);
/// The share of one core at which a worker counts as busy.
const BUSY: f64 = 0.9;
/// How many samples in a row every worker has to be busy for.
const BUSY_SAMPLES: u32 = 2;
/// How much idle CPU, in CPUs, there has to be for one more worker.
const SPARE_CPU: f64 = 0.5;
/// How many calls in flight for each worker count as piling up.
const BACKLOG: usize = 128;
/// How many samples a worker on trial is judged over.
const TRIAL_SAMPLES: u32 = 4;
/// How much higher the call rate has to be with the worker on trial.
const TRIAL_GAIN: f64 = 1.1;
/// A gain that settles it early, after [`BUSY_SAMPLES`] samples.
const TRIAL_CLEAR_GAIN: f64 = 1.3;
/// How long a trial waits for its worker to start before giving up.
const TRIAL_START: Duration = Duration::from_secs(15);
/// How long the pool waits after a worker didn't help before it tries
/// another, at first and at most. It doubles each time.
const WAIT_FIRST: Duration = Duration::from_secs(60);
const WAIT_MOST: Duration = Duration::from_secs(16 * 60);

/// What to do with the pool after a sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Hold,
    /// Add a worker.
    Grow,
    /// Retire a worker: the others have room for its work.
    Shrink,
    /// Retire the worker that was on trial: it didn't help.
    Withdraw,
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
    /// Calls started, per second.
    pub rate: f64,
    /// Calls in flight at the end of the sample, over all workers.
    pub in_flight: usize,
}

/// A worker added because the others were busy, until it has shown that
/// it helps.
#[derive(Debug)]
struct Trial {
    /// The call rate before it was added.
    before: f64,
    /// How many workers there were then.
    workers: usize,
    since: Instant,
    samples: u32,
    rate_sum: f64,
}

/// The state between samples.
#[derive(Debug)]
pub struct Scaler {
    /// How long the pool has to have a worker to spare before one is
    /// retired.
    spare_for: Duration,
    busy_samples: u32,
    /// The call rates of those busy samples, added up.
    busy_rate_sum: f64,
    spare_since: Option<Instant>,
    trial: Option<Trial>,
    /// No trials before this: the last one didn't help.
    wait_until: Option<Instant>,
    /// How long to wait after the next one that doesn't.
    wait: Duration,
}

impl Scaler {
    pub fn new(spare_for: Duration) -> Scaler {
        Scaler {
            spare_for,
            busy_samples: 0,
            busy_rate_sum: 0.0,
            spare_since: None,
            trial: None,
            wait_until: None,
            wait: WAIT_FIRST,
        }
    }

    /// Takes one sample of a pool that keeps between `min` and `max`
    /// workers, and says what to do with it.
    pub fn step(&mut self, sample: Sample, min: usize, max: usize, now: Instant) -> Decision {
        let n = sample.loads.len();
        let known: Vec<f64> = sample.loads.iter().flatten().copied().collect();
        let all_known = n > 0 && known.len() == n;

        if let Some(trial) = &mut self.trial {
            // Wait for the new worker, then for a sample of its own.
            if n <= trial.workers {
                if now.duration_since(trial.since) >= TRIAL_START {
                    self.trial = None;
                }
                return Decision::Hold;
            }
            if !all_known {
                return Decision::Hold;
            }
            trial.samples += 1;
            trial.rate_sum += sample.rate;
            let rate = trial.rate_sum / f64::from(trial.samples);
            let clear = trial.samples >= BUSY_SAMPLES && rate >= trial.before * TRIAL_CLEAR_GAIN;
            if trial.samples < TRIAL_SAMPLES && !clear {
                return Decision::Hold;
            }
            let helped = rate >= trial.before * TRIAL_GAIN;
            self.trial = None;
            self.busy_samples = 0;
            self.busy_rate_sum = 0.0;
            if !helped {
                self.wait_until = Some(now + self.wait);
                self.wait = (self.wait * 2).min(WAIT_MOST);
                return Decision::Withdraw;
            }
            self.wait = WAIT_FIRST;
            // Still all busy: the next one goes on trial against this rate.
            let room = sample.idle_cpus.is_none_or(|idle| idle >= SPARE_CPU);
            if n < max && room && known.iter().all(|load| *load >= BUSY) {
                self.trial = Some(Trial {
                    before: rate,
                    workers: n,
                    since: now,
                    samples: 0,
                    rate_sum: 0.0,
                });
                return Decision::Grow;
            }
            return Decision::Hold;
        }

        if all_known && known.iter().all(|load| *load >= BUSY) {
            self.busy_samples += 1;
            self.busy_rate_sum += sample.rate;
        } else {
            self.busy_samples = 0;
            self.busy_rate_sum = 0.0;
        }
        let room = sample.idle_cpus.is_none_or(|idle| idle >= SPARE_CPU);
        // More arriving than is answered needs no trial, and doesn't wait.
        let piled = sample.in_flight >= BACKLOG * n;
        let free = self.wait_until.is_none_or(|until| now >= until);
        if n < max && room && self.busy_samples >= BUSY_SAMPLES && (piled || free) {
            if !piled {
                self.trial = Some(Trial {
                    before: self.busy_rate_sum / f64::from(self.busy_samples),
                    workers: n,
                    since: now,
                    samples: 0,
                    rate_sum: 0.0,
                });
            }
            self.busy_samples = 0;
            self.busy_rate_sum = 0.0;
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

/// The CPU time a process and its descendants have used, in ticks.
///
/// A runtime's command runs through `sh -c`. The shell usually replaces
/// itself with the worker, but not for a command such as `cd x && node
/// worker.js`, where the worker is the shell's child.
pub fn tree_ticks(pid: u32) -> Option<u64> {
    fn walk(pid: u32, depth: usize, total: &mut u64) -> Option<()> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        *total += stat_ticks(&stat)?;
        if depth == 0 {
            return Some(());
        }
        // Each thread lists the children it started. A kernel built
        // without this file reports the process alone.
        let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
            return Some(());
        };
        for task in tasks.flatten() {
            let Ok(children) = std::fs::read_to_string(task.path().join("children")) else {
                continue;
            };
            for child in children.split_ascii_whitespace() {
                if let Ok(child) = child.parse() {
                    walk(child, depth - 1, total);
                }
            }
        }
        Some(())
    }
    let mut total = 0;
    walk(pid, 4, &mut total)?;
    Some(total)
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

    /// A sample of workers with these loads, answering `rate` calls a
    /// second.
    fn at(loads: &[Option<f64>], rate: f64) -> Sample<'_> {
        Sample {
            loads,
            rate,
            ..Sample::default()
        }
    }

    fn of(loads: &[Option<f64>]) -> Sample<'_> {
        at(loads, 1000.0)
    }

    /// Steps a pool of one to four workers.
    fn step(s: &mut Scaler, sample: Sample, now: Instant) -> Decision {
        s.step(sample, 1, 4, now)
    }

    /// Runs a trial to its end: the new worker's first sample, then
    /// `samples` it's judged over at `rate`. Returns the verdict.
    fn trial_over(
        s: &mut Scaler,
        loads: &[Option<f64>],
        rate: f64,
        samples: u32,
        now: Instant,
    ) -> Decision {
        let mut starting = loads.to_vec();
        *starting.last_mut().unwrap() = None;
        assert_eq!(step(s, of(&starting), now), Decision::Hold);
        for _ in 1..samples {
            assert_eq!(step(s, at(loads, rate), now), Decision::Hold);
        }
        step(s, at(loads, rate), now)
    }

    fn trial(s: &mut Scaler, loads: &[Option<f64>], rate: f64, now: Instant) -> Decision {
        trial_over(s, loads, rate, TRIAL_SAMPLES, now)
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
    fn a_worker_that_adds_calls_stays() {
        let mut s = Scaler::default();
        let now = Instant::now();
        let one = [Some(1.0)];
        assert_eq!(step(&mut s, at(&one, 700.0), now), Decision::Hold);
        assert_eq!(step(&mut s, at(&one, 720.0), now), Decision::Grow);
        // The pool waits for it to start.
        assert_eq!(step(&mut s, at(&one, 710.0), now), Decision::Hold);
        // Twice the workers answer nearly twice the calls. That's clear
        // after two samples, and with both still busy, a third is tried.
        let two = [Some(1.0), Some(1.0)];
        assert_eq!(trial_over(&mut s, &two, 1400.0, 2, now), Decision::Grow);
        // A third adds a sixth, which takes the whole trial to tell, and
        // then the three have time to spare.
        let three = [Some(0.8), Some(0.8), Some(0.8)];
        assert_eq!(trial(&mut s, &three, 1650.0, now), Decision::Hold);
        for _ in 0..10 {
            assert_eq!(step(&mut s, at(&three, 1650.0), now), Decision::Hold);
        }
    }

    #[test]
    fn a_worker_that_adds_none_is_retired_and_the_next_try_waits() {
        let mut s = Scaler::default();
        let start = Instant::now();
        let one = [Some(1.0)];
        let two = [Some(0.5), Some(0.5)];
        assert_eq!(step(&mut s, at(&one, 70_000.0), start), Decision::Hold);
        assert_eq!(step(&mut s, at(&one, 71_000.0), start), Decision::Grow);
        // Two workers, the same calls: something else is the limit.
        assert_eq!(trial(&mut s, &two, 70_000.0, start), Decision::Withdraw);

        // The one worker is as busy as before, and that's no reason now.
        let soon = start + WAIT_FIRST - Duration::from_secs(1);
        for _ in 0..10 {
            assert_eq!(step(&mut s, at(&one, 71_000.0), soon), Decision::Hold);
        }
        // After the wait it's tried again, and then the wait is longer.
        let later = start + WAIT_FIRST;
        assert_eq!(step(&mut s, at(&one, 71_000.0), later), Decision::Grow);
        assert_eq!(trial(&mut s, &two, 70_000.0, later), Decision::Withdraw);
        let not_yet = later + WAIT_FIRST;
        for _ in 0..3 {
            assert_eq!(step(&mut s, at(&one, 71_000.0), not_yet), Decision::Hold);
        }
        let again = later + WAIT_FIRST * 2;
        assert_eq!(step(&mut s, at(&one, 71_000.0), again), Decision::Grow);
    }

    #[test]
    fn calls_piling_up_need_no_trial_and_no_wait() {
        let mut s = Scaler::default();
        let start = Instant::now();
        let one = [Some(1.0)];
        let two = [Some(0.5), Some(0.5)];
        assert_eq!(step(&mut s, at(&one, 1000.0), start), Decision::Hold);
        assert_eq!(step(&mut s, at(&one, 1000.0), start), Decision::Grow);
        assert_eq!(trial(&mut s, &two, 1000.0, start), Decision::Withdraw);

        // More arrive than one worker answers: they pile up in it.
        let piled = Sample {
            in_flight: BACKLOG,
            ..at(&one, 1000.0)
        };
        assert_eq!(step(&mut s, piled, start), Decision::Hold);
        assert_eq!(step(&mut s, piled, start), Decision::Grow);
        // That worker isn't on trial: it stays whatever the rate does.
        let mut starting = two;
        starting[1] = None;
        assert_eq!(step(&mut s, of(&starting), start), Decision::Hold);
        for _ in 0..TRIAL_SAMPLES * 2 {
            assert_eq!(step(&mut s, at(&two, 1000.0), start), Decision::Hold);
        }
    }

    #[test]
    fn a_trial_whose_worker_never_starts_is_dropped() {
        let mut s = Scaler::default();
        let start = Instant::now();
        let one = [Some(1.0)];
        assert_eq!(step(&mut s, of(&one), start), Decision::Hold);
        assert_eq!(step(&mut s, of(&one), start), Decision::Grow);
        assert_eq!(
            step(&mut s, of(&one), start + Duration::from_secs(5)),
            Decision::Hold
        );
        let later = start + TRIAL_START;
        assert_eq!(step(&mut s, of(&one), later), Decision::Hold);
        // The pool is free to try again.
        assert_eq!(step(&mut s, of(&one), later), Decision::Hold);
        assert_eq!(step(&mut s, of(&one), later), Decision::Grow);
    }

    #[test]
    fn never_grows_past_the_most_allowed() {
        let mut s = Scaler::default();
        let now = Instant::now();
        let full = [Some(1.0); 4];
        for _ in 0..10 {
            let piled = Sample {
                in_flight: 10_000,
                waited: true,
                ..of(&full)
            };
            assert_eq!(step(&mut s, piled, now), Decision::Hold);
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
    }
}
