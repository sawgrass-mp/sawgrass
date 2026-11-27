//! One resource: its Luau instance, its event handlers and the limits every call into it runs under.

use std::collections::VecDeque;
use std::fmt;
use std::path::{Component, Path};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use mlua::chunk::Compiler;
use mlua::{Function, IntoLuaMulti, Lua, Table, Value, Variadic, VmState};

use crate::CLOCK_EVERY;
use crate::guards;
use crate::host::LoadError;

const DEADLINE_MESSAGE: &str = "script deadline exceeded";
/// The longest error message kept, in bytes.
const MAX_MESSAGE: usize = 1024;

/// What every call into a resource runs under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The longest one event may run in one resource, all its handlers together.
    pub deadline: Duration,
    /// The longest loading may run: each script's top-level code, once, when the resource starts.
    pub load_deadline: Duration,
    /// The most memory the resource's Luau instance may hold, in bytes. It also bounds the longest stall one
    /// built-in can cause, about 1 ms per MiB (see [`crate::guards`]).
    pub memory: usize,
    /// Log lines kept until the host drains them; older lines are dropped first.
    pub log_lines: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            deadline: Duration::from_millis(4),
            load_deadline: Duration::from_secs(1),
            memory: 16 << 20,
            log_lines: 1000,
        }
    }
}

/// An event the server delivers to resources.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Event {
    /// A tick ran: its number and the seconds since the previous one.
    Tick { tick: u64, dt: f64 },
    /// A player entered the world, by their entity id.
    PlayerJoin(u32),
    /// A player left the world, by their entity id.
    PlayerLeave(u32),
}

const EVENTS: [&str; 3] = ["tick", "playerJoin", "playerLeave"];

impl Event {
    /// The name scripts pass to `sawgrass.on`.
    #[must_use]
    pub fn name(&self) -> &'static str {
        EVENTS[self.index()]
    }

    fn index(&self) -> usize {
        match self {
            Event::Tick { .. } => 0,
            Event::PlayerJoin(_) => 1,
            Event::PlayerLeave(_) => 2,
        }
    }
}

/// Why a call into a resource failed. The resource stays usable either way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallError {
    /// The call ran past its deadline and was stopped.
    Deadline,
    /// The resource ran into its memory cap.
    Memory,
    /// The script raised an error (message cut to 1 KiB).
    Script(String),
}

impl fmt::Display for CallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CallError::Deadline => f.write_str(DEADLINE_MESSAGE),
            CallError::Memory => f.write_str("script memory cap reached"),
            CallError::Script(m) => write!(f, "script error: {m}"),
        }
    }
}

impl std::error::Error for CallError {}

/// Counters since the resource loaded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// Events delivered.
    pub calls: u64,
    /// Calls stopped by the deadline.
    pub deadlines: u64,
    /// Calls that ran into the memory cap.
    pub memory_errors: u64,
    /// Calls in which a handler raised an error.
    pub script_errors: u64,
    /// Time spent in the resource's handlers.
    pub busy: Duration,
    /// The longest single call. It can pass the deadline inside a built-in the interrupt does not reach (see
    /// [`crate::guards`]).
    pub slowest: Duration,
    /// Tick events skipped because the host's script budget for that tick was spent.
    pub skipped_ticks: u64,
}

/// The deadline the interrupt callback enforces. Only the thread calling into the resource touches it; it is
/// atomic (relaxed) only so that a resource can move between threads.
#[derive(Debug)]
struct Clock {
    base: Instant,
    /// Nanoseconds after `base`; `u64::MAX` when disarmed.
    deadline: AtomicU64,
    count: AtomicU32,
    fired: AtomicBool,
}

impl Clock {
    fn new() -> Self {
        Self {
            base: Instant::now(),
            deadline: AtomicU64::new(u64::MAX),
            count: AtomicU32::new(0),
            fired: AtomicBool::new(false),
        }
    }

    fn now(&self) -> u64 {
        u64::try_from(self.base.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }

    fn arm(&self, budget: Duration) {
        let budget = u64::try_from(budget.as_nanos()).unwrap_or(u64::MAX);
        self.deadline.store(self.now().saturating_add(budget), Relaxed);
        self.count.store(0, Relaxed);
        self.fired.store(false, Relaxed);
    }

    /// Disarms the clock and says whether it stopped the script.
    fn disarm(&self) -> bool {
        self.deadline.store(u64::MAX, Relaxed);
        self.fired.swap(false, Relaxed)
    }

    fn fired(&self) -> bool {
        self.fired.load(Relaxed)
    }

    fn check(&self) -> mlua::Result<VmState> {
        if !self.fired() {
            let n = self.count.load(Relaxed) + 1;
            if n < CLOCK_EVERY {
                self.count.store(n, Relaxed);
                return Ok(VmState::Continue);
            }
            self.count.store(0, Relaxed);
            if self.now() < self.deadline.load(Relaxed) {
                return Ok(VmState::Continue);
            }
            self.fired.store(true, Relaxed);
        }
        Err(mlua::Error::runtime(DEADLINE_MESSAGE))
    }
}

type Log = Arc<Mutex<VecDeque<String>>>;

/// A loaded resource.
#[derive(Debug)]
pub struct Resource {
    name: String,
    lua: Lua,
    handlers: Table,
    /// Handlers registered per event, in [`EVENTS`] order, so an event nobody listens to costs nothing.
    counts: Arc<[AtomicU32; EVENTS.len()]>,
    clock: Arc<Clock>,
    log: Log,
    limits: Limits,
    stats: Stats,
    last_error: Option<CallError>,
}

impl Resource {
    /// Starts a resource from source files already read: `(path, source)` pairs, run in order.
    pub fn from_sources(name: &str, sources: &[(String, String)], limits: Limits) -> Result<Self, LoadError> {
        let fail = |e: mlua::Error| LoadError::new(name, e.to_string());
        let lua = Lua::new();
        lua.set_compiler(Compiler::new().set_optimization_level(2));
        lua.enable_jit(true);

        let g = lua.globals();
        for unsafe_global in ["require", "loadstring", "getfenv", "setfenv"] {
            g.raw_set(unsafe_global, Value::Nil).map_err(fail)?;
        }
        let log = Log::default();
        let sink = Arc::clone(&log);
        let keep = limits.log_lines;
        let print = lua
            .create_function(move |lua, args: Variadic<Value>| {
                let mut line = String::new();
                for (i, v) in args.iter().enumerate() {
                    if i > 0 {
                        line.push('\t');
                    }
                    line.push_str(
                        &lua.coerce_string(v.clone())?
                            .map_or_else(|| format!("{v:?}"), |s| s.to_string_lossy()),
                    );
                }
                let mut log = sink.lock().unwrap_or_else(PoisonError::into_inner);
                if log.len() >= keep {
                    log.pop_front();
                }
                log.push_back(cut(line));
                Ok(())
            })
            .map_err(fail)?;
        g.raw_set("print", print).map_err(fail)?;
        guards::install(&lua).map_err(fail)?;

        let handlers = lua.create_table().map_err(fail)?;
        for event in EVENTS {
            handlers
                .raw_set(event, lua.create_table().map_err(fail)?)
                .map_err(fail)?;
        }
        let api = lua.create_table().map_err(fail)?;
        let h = handlers.clone();
        let counts: Arc<[AtomicU32; EVENTS.len()]> = Arc::default();
        let c = Arc::clone(&counts);
        let on = lua
            .create_function(move |_, (event, handler): (String, Function)| {
                let Some(i) = EVENTS.iter().position(|e| *e == event) else {
                    return Err(mlua::Error::runtime(format!("unknown event {event:?}")));
                };
                h.raw_get::<Table>(event)?.raw_push(handler)?;
                c[i].fetch_add(1, Relaxed);
                Ok(())
            })
            .map_err(fail)?;
        api.raw_set("on", on).map_err(fail)?;
        g.raw_set("sawgrass", api).map_err(fail)?;
        lua.sandbox(true).map_err(fail)?;

        lua.set_memory_limit(limits.memory).map_err(fail)?;
        let clock = Arc::new(Clock::new());
        let c = Arc::clone(&clock);
        lua.set_interrupt(move |_| c.check());

        let resource = Self {
            name: name.to_owned(),
            lua,
            handlers,
            counts,
            clock,
            log,
            limits,
            stats: Stats::default(),
            last_error: None,
        };
        for (path, source) in sources {
            resource.clock.arm(limits.load_deadline);
            let result = resource
                .lua
                .load(source.as_str())
                .set_name(format!("@{name}/{path}"))
                .exec();
            let fired = resource.clock.disarm();
            if let Err(e) = classify(result, fired) {
                return Err(LoadError::new(name, format!("{path}: {e}")));
            }
        }
        Ok(resource)
    }

    /// The limits its calls run under.
    #[must_use]
    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// The resource's name (its folder's name).
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Counters since the resource loaded.
    #[must_use]
    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// The most recent failed call's error.
    #[must_use]
    pub fn last_error(&self) -> Option<&CallError> {
        self.last_error.as_ref()
    }

    pub(crate) fn skip_tick(&mut self) {
        self.stats.skipped_ticks += 1;
    }

    /// Whether the resource has a handler for this event.
    #[must_use]
    pub fn listens(&self, event: &Event) -> bool {
        self.counts[event.index()].load(Relaxed) > 0
    }

    /// Bytes its Luau instance holds now.
    #[must_use]
    pub fn memory_used(&self) -> usize {
        self.lua.used_memory()
    }

    /// Takes the lines the resource printed since the last call.
    pub fn drain_log(&mut self) -> Vec<String> {
        self.log
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .drain(..)
            .collect()
    }

    /// Delivers an event to every handler the resource registered for it, in registration order, under one
    /// deadline. A handler's script error does not stop the later handlers; a deadline or memory error does. The
    /// first error is returned.
    pub fn dispatch(&mut self, event: Event) -> Result<(), CallError> {
        self.dispatch_within(event, self.limits.deadline)
    }

    /// [`Self::dispatch`] with a deadline no longer than `budget`.
    pub fn dispatch_within(&mut self, event: Event, budget: Duration) -> Result<(), CallError> {
        if !self.listens(&event) {
            return Ok(());
        }
        let start = Instant::now();
        self.clock.arm(self.limits.deadline.min(budget));
        let result = match event {
            Event::Tick { tick, dt } => self.call_all(event.name(), (tick, dt)),
            Event::PlayerJoin(p) | Event::PlayerLeave(p) => self.call_all(event.name(), p),
        };
        let fired = self.clock.disarm();
        let result = classify(result, fired);
        let took = start.elapsed();
        self.stats.calls += 1;
        self.stats.busy += took;
        self.stats.slowest = self.stats.slowest.max(took);
        if let Err(e) = &result {
            match e {
                CallError::Deadline => self.stats.deadlines += 1,
                CallError::Memory => self.stats.memory_errors += 1,
                CallError::Script(_) => self.stats.script_errors += 1,
            }
            self.last_error = Some(e.clone());
        }
        result
    }

    fn call_all(&self, event: &str, args: impl IntoLuaMulti + Clone) -> mlua::Result<()> {
        let list: Table = self.handlers.raw_get(event)?;
        let mut first = Ok(());
        for i in 1..=list.raw_len() {
            let f: Function = list.raw_get(i)?;
            if let Err(e) = f.call::<()>(args.clone()) {
                if self.clock.fired() || is_memory(&e) {
                    return Err(e);
                }
                if first.is_ok() {
                    first = Err(e);
                }
            }
        }
        first
    }
}

/// Checks a resource folder's relative script path: no absolute paths, no `..`, Luau sources only.
pub(crate) fn check_script_path(path: &str) -> bool {
    let p = Path::new(path);
    p.components().all(|c| matches!(c, Component::Normal(_)))
        && p.extension().is_some_and(|e| e == "luau" || e == "lua")
}

fn is_memory(e: &mlua::Error) -> bool {
    match e {
        mlua::Error::MemoryError(_) => true,
        mlua::Error::CallbackError { cause, .. } => is_memory(cause),
        _ => false,
    }
}

fn classify<T>(result: mlua::Result<T>, fired: bool) -> Result<(), CallError> {
    if fired {
        return Err(CallError::Deadline);
    }
    match result {
        Ok(_) => Ok(()),
        Err(e) if is_memory(&e) => Err(CallError::Memory),
        Err(e) => Err(CallError::Script(cut(e.to_string()))),
    }
}

fn cut(mut s: String) -> String {
    if s.len() > MAX_MESSAGE {
        let mut end = MAX_MESSAGE;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
    s
}
