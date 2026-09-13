//! What amux is costing: its own process, plus the ones it started.
//!
//! The count covers the children too — a pane preview is an `rmux
//! attach-session` of its own and the file browser is a whole `yazi`, and
//! neither would be running if amux had not started it. On this machine the
//! process alone is 12.6 MB against 21.3 MB with one pane open, so reporting
//! only the former would understate what is actually being spent by two
//! thirds.
//!
//! Sampled by syscall rather than by shelling out to `ps`: a tool that reports
//! its own cost should not spend much producing the number, and `ps` reports a
//! cpu figure averaged over the whole life of the process, which for something
//! left open all day is not a reading of anything.

use std::time::Instant;

/// A reading over the interval since the previous one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Usage {
    /// Percent of one core, over the interval between the last two samples.
    /// None on the first sample — there is no interval yet to average over.
    pub cpu: Option<f32>,
    /// Resident bytes, summed across the processes.
    pub rss: u64,
    /// How many processes that covers, amux included.
    pub procs: usize,
}

/// Holds the previous reading, which is what makes a cpu percentage possible.
#[derive(Debug, Default)]
pub struct Sampler {
    previous: Option<(Instant, u64)>,
}

impl Sampler {
    /// Read amux and `children`, whose pids are the ones it spawned.
    ///
    /// Returns None where the platform has no way to ask — the footer then
    /// shows nothing rather than a zero, which would read as "costs nothing".
    pub fn sample(&mut self, children: &[u32]) -> Option<Usage> {
        let now = Instant::now();
        let mut cpu_ns = 0u64;
        let mut rss = 0u64;
        let mut procs = 0usize;

        for pid in std::iter::once(std::process::id()).chain(children.iter().copied()) {
            // A child that has just exited is not an error: it is gone, and the
            // remaining ones still add up to something worth showing.
            if let Some((cpu, bytes)) = probe(pid) {
                cpu_ns += cpu;
                rss += bytes;
                procs += 1;
            }
        }
        if procs == 0 {
            return None;
        }

        let cpu = self.previous.and_then(|(then, before)| {
            let elapsed = now.duration_since(then).as_nanos();
            // A child exiting between samples takes its accumulated time with
            // it, so the total can fall. That is not negative cpu use; it is a
            // different set of processes, and there is nothing to report.
            let spent = cpu_ns.checked_sub(before)?;
            (elapsed > 0).then(|| spent as f32 / elapsed as f32 * 100.0)
        });
        self.previous = Some((now, cpu_ns));

        Some(Usage { cpu, rss, procs })
    }
}

/// Cumulative cpu nanoseconds and resident bytes for one process.
#[cfg(target_os = "macos")]
fn probe(pid: u32) -> Option<(u64, u64)> {
    // SAFETY: the struct is zeroed before the call and its size is passed, so
    // the kernel writes only within it. A pid that has exited returns <= 0,
    // which is checked before anything is read back out.
    unsafe {
        let mut info: libc::proc_taskinfo = std::mem::zeroed();
        let size = std::mem::size_of::<libc::proc_taskinfo>() as i32;
        let written = libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDTASKINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        );
        if written != size {
            return None;
        }
        let ticks = (info.pti_total_user + info.pti_total_system) as u128;
        Some(((ticks * timebase() / 1000) as u64, info.pti_resident_size))
    }
}

/// Nanoseconds per thousand mach time units.
///
/// `pti_total_user` is documented in nanoseconds and is not: it counts mach
/// absolute time, which on this machine ticks every 41.667ns. Reading it as
/// nanoseconds reported amux at 0.1% against `top`'s 4.3% — low by exactly the
/// timebase, and low in a direction that flatters the thing doing the
/// reporting. Scaled by a thousand so the ratio survives integer division.
#[cfg(target_os = "macos")]
fn timebase() -> u128 {
    use std::sync::OnceLock;
    static CACHED: OnceLock<u128> = OnceLock::new();
    *CACHED.get_or_init(|| {
        // SAFETY: writes into a zeroed struct of the size it expects.
        let info = unsafe {
            let mut info: libc::mach_timebase_info = std::mem::zeroed();
            libc::mach_timebase_info(&mut info);
            info
        };
        if info.denom == 0 {
            return 1000;
        }
        1000 * info.numer as u128 / info.denom as u128
    })
}

#[cfg(target_os = "linux")]
fn probe(pid: u32) -> Option<(u64, u64)> {
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if ticks <= 0 || page <= 0 {
        return None;
    }
    let cpu = cpu_from_stat(
        &std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?,
        ticks as u64,
    )?;
    // Second field of statm is the resident set, in pages.
    let pages: u64 = std::fs::read_to_string(format!("/proc/{pid}/statm"))
        .ok()?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    Some((cpu, pages * page as u64))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn probe(_pid: u32) -> Option<(u64, u64)> {
    None
}

/// Cumulative cpu nanoseconds out of a `/proc/<pid>/stat` line.
///
/// Split at the last `)` rather than on whitespace from the start: the second
/// field is the executable name, and it is neither quoted nor escaped, so a
/// process called `foo bar) baz` would otherwise shift every field after it.
#[cfg(any(target_os = "linux", test))]
fn cpu_from_stat(line: &str, ticks: u64) -> Option<u64> {
    let after_name = &line[line.rfind(')')? + 1..];
    let fields: Vec<&str> = after_name.split_whitespace().collect();
    // The first field here is `state`, the third of the line, so utime (the
    // fourteenth) and stime (the fifteenth) sit at these offsets.
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    Some((utime + stime) * 1_000_000_000 / ticks)
}

/// The footer line, trimmed to what `width` allows.
///
/// Dropped from the right as room runs out: the process count is context, the
/// memory is the number people go looking for, and the name has to stay or the
/// line is just a pair of anonymous figures at the bottom of the tree.
pub fn summarise(usage: &Usage, width: u16) -> String {
    let cpu = match usage.cpu {
        Some(pct) => format!("{pct:.1}%"),
        None => "—".to_string(),
    };
    let mb = usage.rss as f64 / 1024.0 / 1024.0;
    let mem = if mb >= 100.0 {
        format!("{mb:.0} MB")
    } else {
        format!("{mb:.1} MB")
    };

    let full = format!("amux  {cpu}  {mem}  {} procs", usage.procs);
    if full.chars().count() <= width as usize {
        return full;
    }
    let short = format!("amux  {cpu}  {mem}");
    if short.chars().count() <= width as usize {
        return short;
    }
    mem
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reading has to be of this process, not of nothing.
    ///
    /// Runs on every platform CI builds for, which is the point: the macOS and
    /// Linux probes are different code, and a platform with neither must say so
    /// by returning None rather than by reporting an empty process.
    #[test]
    fn sampling_reads_this_process() {
        let mut sampler = Sampler::default();
        let first = sampler.sample(&[]);

        if cfg!(any(target_os = "macos", target_os = "linux")) {
            let first = first.expect("this process must be readable");
            assert_eq!(first.procs, 1);
            assert!(first.rss > 0, "a running process holds some memory");
            assert!(first.cpu.is_none(), "one sample cannot give a rate");

            // Something to measure, so the second sample has an interval and
            // some work inside it.
            let mut n = 0u64;
            for i in 0..2_000_000u64 {
                n = n.wrapping_add(i);
            }
            assert!(n > 0);

            let second = sampler.sample(&[]).expect("still running");
            let cpu = second.cpu.expect("a second sample gives a rate");
            assert!(cpu >= 0.0 && cpu < 10_000.0, "implausible cpu reading: {cpu}");
        } else {
            assert!(first.is_none(), "a platform we cannot read must say so");
        }
    }

    /// The cpu figure has to agree with the kernel's other answer for it.
    ///
    /// macOS reports this in mach time units, not the nanoseconds its field
    /// name suggests, and the two differ by 41.7× on this machine — which
    /// showed up as amux claiming 0.1% while `top` said 4.3%. Nothing about
    /// the number looks wrong on its own; it only fails against a second
    /// source, so that is what this checks.
    #[cfg(unix)]
    #[test]
    fn the_cpu_figure_agrees_with_getrusage() {
        fn rusage_seconds() -> f64 {
            // SAFETY: writes into a zeroed struct of its own type.
            let ru = unsafe {
                let mut ru: libc::rusage = std::mem::zeroed();
                libc::getrusage(libc::RUSAGE_SELF, &mut ru);
                ru
            };
            ru.ru_utime.tv_sec as f64
                + ru.ru_utime.tv_usec as f64 / 1e6
                + ru.ru_stime.tv_sec as f64
                + ru.ru_stime.tv_usec as f64 / 1e6
        }

        // Enough work that the comparison is not between two roundings.
        let mut n = 0u64;
        for i in 0..40_000_000u64 {
            n = n.wrapping_add(i ^ (i >> 3));
        }
        assert!(n > 0);

        let (ours, _) = probe(std::process::id()).expect("this process is readable");
        let theirs = rusage_seconds();
        let ours = ours as f64 / 1e9;

        assert!(theirs > 0.005, "the test did not do enough work to compare");
        let ratio = ours / theirs;
        assert!(
            (0.7..1.4).contains(&ratio),
            "cpu disagrees with getrusage: {ours:.4}s against {theirs:.4}s ({ratio:.2}x)"
        );
    }

    /// A pid that is gone must not be counted, and must not be an error.
    #[test]
    fn a_dead_process_is_skipped_rather_than_failing() {
        let mut sampler = Sampler::default();
        // Far above the pid ceiling on both platforms.
        let out = sampler.sample(&[4_000_000_000]);
        if cfg!(any(target_os = "macos", target_os = "linux")) {
            assert_eq!(out.expect("self is still readable").procs, 1);
        } else {
            assert!(out.is_none());
        }
    }

    /// The executable name in `/proc/<pid>/stat` is unescaped, so parsing that
    /// splits on whitespace from the start reads the wrong fields entirely —
    /// and a name with a bracket in it is exactly what a browser tab or an
    /// agent's own subprocess tends to have.
    #[test]
    fn proc_stat_survives_a_name_with_spaces_and_brackets() {
        // utime 400, stime 200 at the fourteenth and fifteenth fields.
        let fields: Vec<String> = (3..=15)
            .map(|i| match i {
                14 => "400".to_string(),
                15 => "200".to_string(),
                _ => "0".to_string(),
            })
            .collect();
        let tail = fields.join(" ");

        let plain = format!("1234 (amux) {tail}");
        assert_eq!(cpu_from_stat(&plain, 100), Some(6_000_000_000));

        // 600 ticks at 100 per second is six seconds, whatever the name is.
        let awkward = format!("1234 (we (ird) name) {tail}");
        assert_eq!(cpu_from_stat(&awkward, 100), Some(6_000_000_000));

        // A different tick rate scales it rather than being ignored.
        assert_eq!(cpu_from_stat(&plain, 1000), Some(600_000_000));

        // Truncated input is refused, not guessed at.
        assert_eq!(cpu_from_stat("1234 (amux) S 0 0", 100), None);
        assert_eq!(cpu_from_stat("no brackets here", 100), None);
    }

    #[test]
    fn the_footer_gives_up_its_parts_from_the_right() {
        let usage = Usage { cpu: Some(0.4), rss: 21 * 1024 * 1024, procs: 2 };
        assert_eq!(summarise(&usage, 40), "amux  0.4%  21.0 MB  2 procs");
        assert_eq!(summarise(&usage, 20), "amux  0.4%  21.0 MB");
        assert_eq!(summarise(&usage, 10), "21.0 MB");

        // Before the second sample there is no rate to show, and a zero would
        // read as "using no cpu" rather than "not known yet".
        let first = Usage { cpu: None, rss: 12 * 1024 * 1024, procs: 1 };
        assert!(summarise(&first, 40).contains('—'));

        // Past a hundred megabytes the tenth of a megabyte is noise.
        let big = Usage { cpu: Some(12.25), rss: 1536 * 1024 * 1024, procs: 4 };
        assert_eq!(summarise(&big, 40), "amux  12.2%  1536 MB  4 procs");
    }
}
