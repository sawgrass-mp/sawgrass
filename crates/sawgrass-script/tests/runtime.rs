//! The runtime's promises: misbehaving scripts are stopped, the sandbox holds, and events arrive in order.

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use sawgrass_script::{CallError, Event, Host, Limits, Resource, SORT_UNGUARDED};

fn limits() -> Limits {
    Limits {
        // Generous for a debug build; the misbehaving scripts are still stopped well within the test's 500 ms.
        deadline: Duration::from_millis(100),
        load_deadline: Duration::from_millis(200),
        memory: 4 << 20,
        log_lines: 8,
    }
}

fn resource(source: &str) -> Resource {
    match Resource::from_sources("test", &[("main.luau".into(), source.into())], limits()) {
        Ok(r) => r,
        Err(e) => panic!("{e}"),
    }
}

const TICK: Event = Event::Tick { tick: 1, dt: 0.05 };

/// A resource whose tick handler runs `body`, then sets `ok` so the next tick can show it still works.
fn on_tick(body: &str) -> Resource {
    resource(&format!(
        "local calls = 0
         sawgrass.on('tick', function()
             calls += 1
             if calls == 1 then {body} end
             print('alive', calls)
         end)"
    ))
}

fn stopped_then_alive(body: &str, expected: CallError) {
    let mut r = on_tick(body);
    let start = Instant::now();
    assert_eq!(r.dispatch(TICK), Err(expected), "{body}");
    assert!(
        start.elapsed() < Duration::from_millis(500),
        "{body}: took {:?}",
        start.elapsed()
    );
    assert_eq!(r.dispatch(TICK), Ok(()), "{body}: not usable after the error");
    assert_eq!(r.drain_log(), ["alive\t2"]);
}

#[test]
fn deadline_stops_misbehaving_scripts() {
    for body in [
        "while true do end",
        "local function f() while true do end end; while true do pcall(f) end",
        "while true do xpcall(function() while true do end end, function() while true do end end) end",
        "coroutine.wrap(function() while true do end end)()",
        "local s = string.rep('a', 30) .. 'b'; while true do string.find(s, '(a*)*b') ; string.match(s, string.rep('a?', 25) .. string.rep('a', 25)) end",
        "local t = {} for i = 1, 100000 do t[i] = (i * 7919) % 100000 end; while true do table.sort(t) end",
    ] {
        stopped_then_alive(body, CallError::Deadline);
    }
}

#[test]
fn memory_cap_stops_allocation() {
    for body in [
        "local t = {} while true do t[#t + 1] = string.rep('x', 1000) .. #t end",
        "local s = 'x' while true do s = s .. s end",
        "local t = string.rep('x', 64 * 1024 * 1024)",
        "local b = buffer.create(64 * 1024 * 1024)",
    ] {
        stopped_then_alive(body, CallError::Memory);
    }
}

#[test]
fn deep_recursion_is_an_error() {
    let mut r = on_tick("local function f(n) return f(n + 1) + 1 end f(1)");
    assert!(matches!(
        r.dispatch(TICK),
        Err(CallError::Script(_) | CallError::Memory)
    ));
    assert_eq!(r.dispatch(TICK), Ok(()));
}

#[test]
fn sandbox_holds() {
    for body in [
        "io.open('x')",
        "os.execute('true')",
        "require('x')",
        "loadstring('return 1')()",
        "getfenv(1).x = 1",
        "table.insert = nil",
        "string.rep = nil",
        "sawgrass.on = nil",
        "math.pi = 3",
    ] {
        let mut r = on_tick(body);
        assert!(
            matches!(r.dispatch(TICK), Err(CallError::Script(_))),
            "{body} was allowed"
        );
    }
    // A script's own globals stay its own.
    let mut r = on_tick("x = 1 assert(x == 1)");
    assert_eq!(r.dispatch(TICK), Ok(()));
}

#[test]
fn guarded_sort_keeps_order() {
    let mut r = on_tick(&format!(
        "local t = {{}} for i = 1, {n} do t[i] = ({n} - i) * 3 % 1001 end
         table.sort(t)
         for i = 2, #t do assert(t[i - 1] <= t[i]) end
         local s = {{'b', 'a', 'c'}} table.sort(s, function(a, b) return a > b end)
         assert(table.concat(s) == 'cba')",
        n = SORT_UNGUARDED * 4
    ));
    assert_eq!(r.dispatch(TICK), Ok(()));
}

#[test]
fn events_arrive_in_order() {
    let mut r = resource(
        "sawgrass.on('playerJoin', function(p) print('join', p) end)
         sawgrass.on('tick', function(n, dt) print('tick', n, dt) end)
         sawgrass.on('tick', function() error('second handler') end)
         sawgrass.on('tick', function() print('third') end)
         sawgrass.on('playerLeave', function(p) print('leave', p) end)",
    );
    assert_eq!(r.dispatch(Event::PlayerJoin(0x0003_0001)), Ok(()));
    assert!(
        matches!(r.dispatch(Event::Tick { tick: 7, dt: 0.05 }), Err(CallError::Script(m)) if m.contains("second handler"))
    );
    assert_eq!(r.dispatch(Event::PlayerLeave(5)), Ok(()));
    assert_eq!(
        r.drain_log(),
        ["join\t196609", "tick\t7\t0.05", "third", "leave\t5"]
    );
    let s = r.stats();
    assert_eq!((s.calls, s.script_errors, s.deadlines), (3, 1, 0));
}

#[test]
fn unknown_event_and_endless_load_fail_the_resource() {
    let load = |src: &str| Resource::from_sources("bad", &[("main.luau".into(), src.into())], limits());
    assert!(load("sawgrass.on('tik', function() end)").is_err());
    assert!(load("while true do end").is_err());
    assert!(load("this is not luau").is_err());
}

#[test]
fn log_is_bounded() {
    let mut r = on_tick("for i = 1, 100 do print(i) end");
    r.dispatch(TICK).ok();
    let log = r.drain_log();
    assert_eq!(log.len(), 8);
    assert_eq!(log.last().map(String::as_str), Some("alive\t1"));
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sawgrass-script-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn write(path: PathBuf, text: &str) {
    fs::create_dir_all(path.parent().expect("parent")).expect("dir");
    fs::write(path, text).expect("write");
}

#[test]
fn a_broken_resource_does_not_stop_the_others() {
    let root = temp_dir("load");
    write(
        root.join("a-good/resource.toml"),
        "server = ['server/a.luau', 'server/b.luau']",
    );
    write(root.join("a-good/server/a.luau"), "count = 0");
    write(
        root.join("a-good/server/b.luau"),
        "sawgrass.on('tick', function() count += 1 print(count) end)",
    );
    write(root.join("b-syntax/resource.toml"), "server = ['main.luau']");
    write(root.join("b-syntax/main.luau"), "sawgrass.on(");
    write(
        root.join("c-escape/resource.toml"),
        "server = ['../a-good/server/a.luau']",
    );
    write(root.join("d-manifest/resource.toml"), "server = 'main.luau'");
    write(root.join("e-unknown/resource.toml"), "server = []\nclient = []");
    write(root.join("f-missing/resource.toml"), "server = ['nope.luau']");
    write(root.join("not-a-resource/readme.txt"), "");

    let (mut host, errors) = Host::load_dir(&root, limits());
    let failed: Vec<_> = errors.iter().map(|e| e.resource.as_str()).collect();
    assert_eq!(
        failed,
        ["b-syntax", "c-escape", "d-manifest", "e-unknown", "f-missing"]
    );
    assert_eq!(host.resources().len(), 1);
    assert!(host.dispatch(TICK).is_empty());
    assert!(host.dispatch(TICK).is_empty());
    assert_eq!(host.resources_mut()[0].drain_log(), ["1", "2"]);
    fs::remove_dir_all(root).ok();
}

#[test]
fn the_repository_resources_load() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../resources");
    let (mut host, errors) = Host::load_dir(&root, Limits::default());
    assert!(errors.is_empty(), "{errors:?}");
    assert!(!host.resources().is_empty());
    assert!(host.dispatch(Event::PlayerJoin(1)).is_empty());
    assert!(host.dispatch(Event::Tick { tick: 600, dt: 0.05 }).is_empty());
    assert!(host.dispatch(Event::PlayerLeave(1)).is_empty());
}

#[test]
fn resources_can_move_to_the_tick_thread() {
    fn is_send<T: Send>() {}
    is_send::<Host>();
    let mut r = on_tick("");
    let handle = std::thread::spawn(move || r.dispatch(TICK));
    assert_eq!(handle.join().ok(), Some(Ok(())));
}
