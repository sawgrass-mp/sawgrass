//! The host's script budget per tick: misbehaving resources cannot take the tick from the others.

use std::time::{Duration, Instant};

use sawgrass_script::{Host, Limits, Resource};

fn resource(name: &str, source: &str) -> Resource {
    let limits = Limits {
        deadline: Duration::from_millis(20),
        memory: 4 << 20,
        ..Limits::default()
    };
    match Resource::from_sources(name, &[("main.luau".into(), source.into())], limits) {
        Ok(r) => r,
        Err(e) => panic!("{e}"),
    }
}

const COUNTER: &str = "local n = 0
    sawgrass.on('playerJoin', function(p) print('join', p) end)
    sawgrass.on('playerLeave', function(p) print('leave', p) end)
    sawgrass.on('tick', function() n += 1 end)";

#[test]
fn player_events_arrive_before_the_tick_in_order() {
    let mut host = Host::default();
    host.push(resource("a", COUNTER));
    let report = host.run_tick(
        0,
        0.05,
        &[0x10001, 0x20001],
        &[0x30001],
        Duration::from_millis(50),
    );
    assert_eq!((report.skipped, report.failed), (0, 0));
    let log = host.resources_mut()[0].drain_log();
    assert_eq!(log, ["join\t65537", "join\t131073", "leave\t196609"]);
    assert_eq!(host.resources()[0].stats().calls, 4);
}

#[test]
fn misbehaving_resources_leave_the_others_running() {
    let mut host = Host::default();
    host.push(resource(
        "loop",
        "sawgrass.on('tick', function() while true do end end)",
    ));
    host.push(resource(
        "memory",
        "sawgrass.on('tick', function() local t = {} while true do t[#t + 1] = {} end end)",
    ));
    host.push(resource(
        "stall",
        "sawgrass.on('tick', function() local s = string.rep('x', 3 * 1024 * 1024) end)",
    ));
    host.push(resource("good", COUNTER));
    let budget = Duration::from_millis(60);
    let mut worst = Duration::ZERO;
    for tick in 0..20 {
        let start = Instant::now();
        host.run_tick(tick, 0.05, &[], &[], budget);
        worst = worst.max(start.elapsed());
    }
    let good = host.resources()[3].stats();
    // The loop and the memory hog take at most their 20 ms deadlines and the stall a few ms, so the good resource
    // still runs every tick within the 60 ms budget, whatever the order.
    assert_eq!((good.calls, good.skipped_ticks), (20, 0));
    assert!(host.resources()[0].stats().deadlines > 0);
    assert!(host.resources()[1].stats().memory_errors + host.resources()[1].stats().deadlines > 0);
    assert!(worst < budget * 3, "worst tick {worst:?}");
}

#[test]
fn a_starved_resource_gets_its_player_events_later() {
    let mut host = Host::default();
    host.push(resource(
        "a-loop",
        "sawgrass.on('tick', function() while true do end end)",
    ));
    host.push(resource("b", COUNTER));
    // A 5 ms budget: on even ticks the loop runs first and spends it all, so `b` waits.
    let budget = Duration::from_millis(5);
    let report = host.run_tick(0, 0.05, &[7], &[], budget);
    assert_eq!(report.skipped, 1);
    assert!(host.resources_mut()[1].drain_log().is_empty());
    host.run_tick(1, 0.05, &[], &[7], budget);
    assert_eq!(host.resources_mut()[1].drain_log(), ["join\t7", "leave\t7"]);
    assert_eq!(host.resources()[1].stats().skipped_ticks, 1);
    assert_eq!(host.dropped(), 0);
}
