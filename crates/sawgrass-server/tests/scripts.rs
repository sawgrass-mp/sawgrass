//! The `scripts` system: resources hear joins and leaves from the roster, then the tick, and report.

use std::sync::mpsc;
use std::time::Duration;

use sawgrass_script::{Limits, Resource};
use sawgrass_server::TickInfo;
use sawgrass_server::net::Roster;
use sawgrass_server::scripts::Scripts;

#[test]
fn resources_hear_the_roster_then_the_tick() {
    let source = "sawgrass.on('playerJoin', function(p) print('join', p) end)
        sawgrass.on('playerLeave', function(p) print('leave', p) end)
        sawgrass.on('tick', function(n) print('tick', n) end)";
    let resource = Resource::from_sources("rp", &[("main.luau".into(), source.into())], Limits::default())
        .expect("resource");
    let mut host = sawgrass_script::Host::default();
    host.push(resource);
    let (tx, rx) = mpsc::channel();
    let mut scripts = Scripts::new(host, Some(rx), Duration::from_millis(50), 0);
    let info = |tick| TickInfo {
        tick,
        dt: Duration::from_millis(50),
    };
    tx.send(Roster::Joined(65537)).expect("send");
    scripts.run_tick(&info(1));
    tx.send(Roster::Left(65537)).expect("send");
    tx.send(Roster::Joined(65538)).expect("send");
    scripts.run_tick(&info(2));

    let report = scripts.report();
    assert!(report[0].starts_with("scripts: slowest tick"), "{report:?}");
    assert!(report[1].starts_with("  rp: 5 calls"), "{report:?}");
    // Joins come before leaves within a tick.
    assert_eq!(
        report[2..],
        [
            "  [rp] join\t65537",
            "  [rp] tick\t1",
            "  [rp] join\t65538",
            "  [rp] leave\t65537",
            "  [rp] tick\t2"
        ]
    );
}
