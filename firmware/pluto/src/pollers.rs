//! Other programs on the board that read and write the AD936x on a timer. While a sweep runs they're paused
//! (SIGSTOP) and resumed afterwards (SIGCONT): their SPI traffic would end fast mode's direct register access,
//! and their debugfs register reads share the driver's one `direct_reg_access` address with ours.
//!
//! On Tezuka firmware: the web dashboard's telemetry loop (`api_controller.sh`: about 100 IIO reads every
//! 2 s, register reads through `direct_reg_access` among them) and the PTT watcher (`watchconsoletx.sh`:
//! reads and rewrites register 0x27 every second). Both are shell scripts; they're found by their exact
//! script path (argv[1]), never by a substring, and only their own processes are signalled: a command they
//! have running finishes, then the bus is waited on until it's quiet.

use std::fs;
use std::time::{Duration, Instant};

const SCRIPTS: &[&str] = &["/root/api_controller.sh", "/root/watchconsoletx.sh"];

/// A process by PID and start time, so a recycled PID is never signalled.
#[derive(Clone, Copy, PartialEq)]
struct Proc {
    pid: i32,
    start: u64,
}

/// Field 22 of /proc/PID/stat (after the parenthesised name, which may hold spaces), and field 3 (the state).
fn stat(pid: i32) -> Option<(char, u64)> {
    let s = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &s[s.rfind(')')? + 2..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    Some((f.first()?.chars().next()?, f.get(19)?.parse().ok()?))
}

fn find() -> Vec<Proc> {
    let mut found = Vec::new();
    let Ok(dir) = fs::read_dir("/proc") else { return found };
    for e in dir.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else { continue };
        let Ok(cmd) = fs::read(format!("/proc/{pid}/cmdline")) else { continue };
        let script = cmd.split(|&b| b == 0).nth(1).and_then(|a| std::str::from_utf8(a).ok());
        if script.is_some_and(|s| SCRIPTS.contains(&s))
            && let Some((_, start)) = stat(pid)
        {
            found.push(Proc { pid, start });
        }
    }
    found
}

fn signal(p: Proc, sig: i32) {
    if stat(p.pid).is_some_and(|(_, start)| start == p.start) {
        unsafe { libc::kill(p.pid, sig) };
    }
}

/// The pollers paused for a sweep; dropping it resumes them.
pub struct Paused(Vec<Proc>);

impl Paused {
    pub fn count(&self) -> usize {
        self.0.len()
    }
}

/// Pause the pollers, then wait (up to a second) for a command one of them had running to finish: until the
/// SPI bus has been quiet for 50 ms.
pub fn pause() -> Paused {
    let procs = find();
    for &p in &procs {
        signal(p, libc::SIGSTOP);
    }
    if !procs.is_empty() {
        let t = Instant::now();
        let mut last = crate::fast::spi_irqs();
        while t.elapsed() < Duration::from_secs(1) {
            std::thread::sleep(Duration::from_millis(50));
            let now = crate::fast::spi_irqs();
            if now == last {
                break;
            }
            last = now;
        }
    }
    Paused(procs)
}

impl Drop for Paused {
    fn drop(&mut self) {
        for &p in &self.0 {
            signal(p, libc::SIGCONT);
        }
    }
}

/// Resume pollers left stopped by an earlier run that ended without resuming them (killed, or aborted).
pub fn resume_stopped() -> usize {
    let stopped: Vec<Proc> = find().into_iter().filter(|p| stat(p.pid).is_some_and(|(s, _)| s == 'T')).collect();
    for &p in &stopped {
        signal(p, libc::SIGCONT);
    }
    stopped.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_its_own_stat() {
        // the process's stat is its main thread's: running or waiting for the test threads
        let (state, start) = stat(std::process::id() as i32).unwrap();
        assert!("RS".contains(state), "{state}");
        assert!(start > 0);
    }
}
