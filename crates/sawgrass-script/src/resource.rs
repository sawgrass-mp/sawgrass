//! One resource: its Luau instance, its event handlers and the limits every call into it runs under.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::fmt;
use std::path::{Component, Path};
use std::rc::Rc;
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
}

/// The deadline the interrupt callback enforces.
#[derive(Debug, Default)]
struct Clock {
    deadline: Cell<Option<Instant>>,
    count: Cell<u32>,
    fired: Cell<bool>,
}

impl Clock {
    fn arm(&self, budget: Duration) {
        self.deadline.set(Some(Instant::now() + budget));
        self.count.set(0);
        self.fired.set(false);
    }

    /// Disarms the clock and says whether it stopped the script.
    fn disarm(&self) -> bool {
        self.deadline.set(None);
        self.fired.replace(false)
    }

    fn check(&self) -> mlua::Result<VmState> {
        if !self.fired.get() {
            let n = self.count.get() + 1;
            if n < CLOCK_EVERY {
                self.count.set(n);
                return Ok(VmState::Continue);
            }
            self.count.set(0);
            match self.deadline.get() {
                Some(d) if Instant::now() >= d => self.fired.set(true),
                _ => return Ok(VmState::Continue),
            }
        }
        Err(mlua::Error::runtime(DEADLINE_MESSAGE))
    }
}

/// A loaded resource.
#[derive(Debug)]
pub struct Resource {
    name: String,
    lua: Lua,
    handlers: Table,
    /// Handlers registered per event, in [`EVENTS`] order, so an event nobody listens to costs nothing.
    counts: Rc<[Cell<u32>; EVENTS.len()]>,
    clock: Rc<Clock>,
    log: Rc<RefCell<VecDeque<String>>>,
    limits: Limits,
    stats: Stats,
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
        let log = Rc::new(RefCell::new(VecDeque::new()));
        let sink = Rc::clone(&log);
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
                let mut log = sink.borrow_mut();
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
        let counts: Rc<[Cell<u32>; EVENTS.len()]> = Rc::default();
        let c = Rc::clone(&counts);
        let on = lua
            .create_function(move |_, (event, handler): (String, Function)| {
                let Some(i) = EVENTS.iter().position(|e| *e == event) else {
                    return Err(mlua::Error::runtime(format!("unknown event {event:?}")));
                };
                h.raw_get::<Table>(event)?.raw_push(handler)?;
                c[i].set(c[i].get() + 1);
                Ok(())
            })
            .map_err(fail)?;
        api.raw_set("on", on).map_err(fail)?;
        g.raw_set("sawgrass", api).map_err(fail)?;
        lua.sandbox(true).map_err(fail)?;

        lua.set_memory_limit(limits.memory).map_err(fail)?;
        let clock = Rc::new(Clock::default());
        let c = Rc::clone(&clock);
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

    /// Bytes its Luau instance holds now.
    #[must_use]
    pub fn memory_used(&self) -> usize {
        self.lua.used_memory()
    }

    /// Takes the lines the resource printed since the last call.
    pub fn drain_log(&mut self) -> Vec<String> {
        self.log.borrow_mut().drain(..).collect()
    }

    /// Delivers an event to every handler the resource registered for it, in registration order, under one
    /// deadline. A handler's script error does not stop the later handlers; a deadline or memory error does. The
    /// first error is returned.
    pub fn dispatch(&mut self, event: Event) -> Result<(), CallError> {
        if self.counts[event.index()].get() == 0 {
            return Ok(());
        }
        let start = Instant::now();
        self.clock.arm(self.limits.deadline);
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
        match &result {
            Ok(()) => {}
            Err(CallError::Deadline) => self.stats.deadlines += 1,
            Err(CallError::Memory) => self.stats.memory_errors += 1,
            Err(CallError::Script(_)) => self.stats.script_errors += 1,
        }
        result
    }

    fn call_all(&self, event: &str, args: impl IntoLuaMulti + Clone) -> mlua::Result<()> {
        let list: Table = self.handlers.raw_get(event)?;
        let mut first = Ok(());
        for i in 1..=list.raw_len() {
            let f: Function = list.raw_get(i)?;
            if let Err(e) = f.call::<()>(args.clone()) {
                if self.clock.fired.get() || is_memory(&e) {
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
