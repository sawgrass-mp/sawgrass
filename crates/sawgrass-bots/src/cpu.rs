//! CPU time of this process and of one of its threads, read from `/proc` on Linux.
//!
//! The bots report it so a run shows whether the load generator itself ran out of CPU (the same code as the
//! server's `cpu` module; the two crates do not depend on each other). Elsewhere the
//! functions return `None`.

use std::time::Duration;

/// Linux reports `utime` and `stime` in clock ticks of `USER_HZ`, which is 100 on every architecture Rust
/// supports there.
const USER_HZ: u64 = 100;

/// User plus system CPU time used by the whole process so far.
#[must_use]
pub fn process_time() -> Option<Duration> {
    read("/proc/self/stat")
}

/// User plus system CPU time used so far by the thread with this kernel thread id. The main thread's id is
/// the process id.
#[must_use]
pub fn thread_time(tid: u32) -> Option<Duration> {
    read(&format!("/proc/self/task/{tid}/stat"))
}

fn read(path: &str) -> Option<Duration> {
    let stat = std::fs::read_to_string(path).ok()?;
    parse_stat(&stat)
}

/// Fields 14 and 15 of a `stat` line. The command name (field 2) is in parentheses and may contain spaces, so
/// counting starts after the last `)`.
fn parse_stat(stat: &str) -> Option<Duration> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut fields = rest.split_whitespace().skip(11);
    let utime: u64 = fields.next()?.parse().ok()?;
    let stime: u64 = fields.next()?.parse().ok()?;
    Some(Duration::from_millis((utime + stime) * 1000 / USER_HZ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_user_and_system_time() {
        let line = "1234 (tick thread) S 1 1234 1234 0 -1 4194560 100 0 0 0 250 50 0 0 20 0 9 0 100 0 0";
        assert_eq!(parse_stat(line), Some(Duration::from_secs(3)));
        assert_eq!(parse_stat("garbage"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn reads_this_process() {
        assert!(process_time().is_some());
        assert!(thread_time(std::process::id()).is_some());
    }
}
