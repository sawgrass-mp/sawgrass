//! Resources that keep overrunning are turned off, and can be turned back on.

use std::time::Duration;

use sawgrass_script::{Host, Limits, Resource};

fn resource(name: &str, source: &str) -> Resource {
    with_limits(
        name,
        source,
        Limits {
            deadline: Duration::from_millis(5),
            memory: 4 << 20,
            ..Limits::default()
        },
    )
}

fn with_limits(name: &str, source: &str, limits: Limits) -> Resource {
    match Resource::from_sources(name, &[("main.luau".into(), source.into())], limits) {
        Ok(r) => r,
        Err(e) => panic!("{e}"),
    }
}

const GOOD: &str = "local n = 0 sawgrass.on('tick', function() n += 1 end)";
/// Loops on every third call.
const FLAKY: &str = "local calls = 0
    sawgrass.on('tick', function()
        calls += 1
        print(calls)
        if calls % 3 == 0 then while true do end end
    end)";
/// Loops on its first `bad` calls, then behaves.
fn bad_then_good(bad: u32) -> String {
    format!(
        "local calls = 0
         sawgrass.on('tick', function()
             calls += 1
             print(calls)
             if calls <= {bad} then while true do end end
         end)"
    )
}

fn host(limit: u32, resources: Vec<Resource>) -> Host {
    let mut host = Host::default();
    host.set_strike_limit(limit);
    for r in resources {
        host.push(r);
    }
    host
}

fn ticks(host: &mut Host, from: u64, n: u64) -> usize {
    (from..from + n)
        .map(|t| {
            host.run_tick(t, 0.05, &[], &[], Duration::from_secs(1))
                .turned_off
        })
        .sum()
}

#[test]
fn a_looping_resource_is_turned_off_and_the_others_run() {
    let mut host = host(
        3,
        vec![resource("good", GOOD), resource("loop", &bad_then_good(1000))],
    );
    assert_eq!(ticks(&mut host, 0, 10), 1);
    assert!(host.is_off(1) && !host.is_off(0));
    assert_eq!(host.resources()[1].stats().deadlines, 3);
    assert_eq!(host.resources()[0].stats().calls, 10);
    let notices = host.take_notices();
    assert_eq!(notices.len(), 1);
    assert!(
        notices[0].starts_with("resource loop turned off after 3 strikes in a row"),
        "{notices:?}"
    );
    assert!(notices[0].contains("deadline exceeded"), "{notices:?}");
}

#[test]
fn failing_now_and_then_is_not_enough() {
    // Every third call loops: never two strikes in a row.
    let mut host = host(2, vec![resource("flaky", FLAKY)]);
    assert_eq!(ticks(&mut host, 0, 12), 0);
    assert_eq!(host.resources()[0].stats().deadlines, 4);
}

#[test]
fn memory_and_stalls_are_strikes() {
    // A 7 MiB string each tick, never the same one (Luau interns strings): a few ms at least, past a 1 ms deadline,
    // in a built-in the interrupt cannot stop.
    let stall = Limits {
        deadline: Duration::from_millis(1),
        ..Limits::default()
    };
    let mut host = host(
        2,
        vec![
            // Two 3 MiB buffers do not fit under the 4 MiB cap.
            resource(
                "memory",
                "sawgrass.on('tick', function() local a, b = buffer.create(3145728), buffer.create(3145728) end)",
            ),
            with_limits(
                "stall",
                "sawgrass.on('tick', function(n) local s = string.rep('x', 7 * 1024 * 1024 - n) end)",
                stall,
            ),
        ],
    );
    assert_eq!(ticks(&mut host, 0, 4), 2);
    assert!(host.is_off(0) && host.is_off(1));
    let notices = host.take_notices();
    assert!(
        notices
            .iter()
            .any(|n| n.starts_with("resource memory") && n.contains("memory cap")),
        "{notices:?}"
    );
    assert!(
        notices
            .iter()
            .any(|n| n.starts_with("resource stall") && n.contains("in one call")),
        "{notices:?}"
    );
}

#[test]
fn with_the_policy_off_nothing_is_turned_off() {
    let mut host = host(0, vec![resource("loop", &bad_then_good(1000))]);
    assert_eq!(ticks(&mut host, 0, 10), 0);
    assert!(!host.is_off(0));
    assert_eq!(host.resources()[0].stats().deadlines, 10);
}

#[test]
fn enable_brings_a_resource_back_with_its_state() {
    let mut host = host(2, vec![resource("r", &bad_then_good(3))]);
    assert_eq!(ticks(&mut host, 0, 5), 1);
    assert!(host.is_off(0));
    assert!(!host.enable("nope"));
    assert!(host.enable("r"));
    assert!(!host.enable("r"), "already on");
    // Its third call loops once more: one strike, not two in a row; then it behaves.
    assert_eq!(ticks(&mut host, 5, 5), 0);
    assert!(!host.is_off(0));
    let log = host.resources_mut()[0].drain_log();
    assert_eq!(log, ["1", "2", "3", "4", "5", "6", "7"]);
    assert_eq!(
        host.take_notices().last().map(String::as_str),
        Some("resource r turned back on")
    );
}
