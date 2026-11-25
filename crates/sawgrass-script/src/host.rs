//! The resources a server runs, loaded from a folder and called in a fixed order.

use std::collections::VecDeque;
use std::fmt;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::resource::{CallError, Event, Limits, Resource, check_script_path};

const MANIFEST: &str = "resource.toml";

/// A resource that failed to load, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadError {
    /// The resource's name.
    pub resource: String,
    /// What went wrong.
    pub message: String,
}

impl LoadError {
    pub(crate) fn new(resource: &str, message: impl Into<String>) -> Self {
        Self {
            resource: resource.to_owned(),
            message: message.into(),
        }
    }
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "resource {}: {}", self.resource, self.message)
    }
}

impl std::error::Error for LoadError {}

/// Player events a resource keeps while it waits for budget; beyond this the oldest are dropped.
const MAX_WAITING: usize = 1 << 16;

/// Strikes in a row that turn a resource off, unless [`Host::set_strike_limit`] says otherwise. Chosen by the
/// project's coordinator, not by the maintainer (SCRIPT-5).
pub const DEFAULT_STRIKES: u32 = 5;

/// Every running resource, called in name order.
#[derive(Debug, Default)]
pub struct Host {
    resources: Vec<Resource>,
    /// What the host keeps per resource, by resource index.
    state: Vec<State>,
    /// Player events dropped because a resource's queue was full.
    dropped: u64,
    /// Strikes in a row that turn a resource off; 0 never does.
    strike_limit: u32,
    /// Lines for the operator: resources turned off and on.
    notices: Vec<String>,
}

#[derive(Debug, Default)]
struct State {
    /// Player events the resource has not had yet.
    waiting: VecDeque<Event>,
    /// Strikes in a row (see [`Host::set_strike_limit`]).
    strikes: u32,
    off: bool,
}

/// What one [`Host::run_tick`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TickReport {
    /// Time spent in resources.
    pub spent: Duration,
    /// Resources that skipped this tick's `tick` event because the budget was spent.
    pub skipped: usize,
    /// Calls that failed.
    pub failed: usize,
    /// Resources turned off this tick (see [`Host::set_strike_limit`]).
    pub turned_off: usize,
}

impl Host {
    /// Loads every folder in `root` that has a `resource.toml`. A resource that fails to load is left out and
    /// reported; the others still run.
    pub fn load_dir(root: &Path, limits: Limits) -> (Self, Vec<LoadError>) {
        let mut dirs: Vec<_> = match fs::read_dir(root) {
            Ok(entries) => entries
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.join(MANIFEST).is_file())
                .collect(),
            Err(e) => {
                let root_name = root.display().to_string();
                return (Self::default(), vec![LoadError::new(&root_name, e.to_string())]);
            }
        };
        dirs.sort();
        let mut host = Self::default();
        let mut errors = Vec::new();
        for dir in dirs {
            match load_resource(&dir, limits) {
                Ok(r) => host.push(r),
                Err(e) => errors.push(e),
            }
        }
        (host, errors)
    }

    /// Adds a resource that is already loaded; it is called after the others.
    pub fn push(&mut self, resource: Resource) {
        self.resources.push(resource);
        self.state.push(State::default());
    }

    /// Turns a resource off after `limit` strikes in a row; 0 never does. A strike is a call that the deadline
    /// stopped while it had its whole per-call deadline, that ran into the memory cap, or that ran past the
    /// per-call deadline inside a built-in. A call that succeeds or raises a script error resets the count. A
    /// resource turned off is not called again, and loses its waiting player events, until [`Host::enable`].
    pub fn set_strike_limit(&mut self, limit: u32) {
        self.strike_limit = limit;
    }

    /// Turns a resource that was turned off back on, keeping its state. Returns whether a resource by that name
    /// was off.
    pub fn enable(&mut self, name: &str) -> bool {
        let Some(i) = self.resources.iter().position(|r| r.name() == name) else {
            return false;
        };
        let state = &mut self.state[i];
        if !state.off {
            return false;
        }
        state.off = false;
        state.strikes = 0;
        self.notices.push(format!("resource {name} turned back on"));
        true
    }

    /// Whether the resource at `index` (as in [`Host::resources`]) is turned off.
    #[must_use]
    pub fn is_off(&self, index: usize) -> bool {
        self.state.get(index).is_some_and(|s| s.off)
    }

    /// Takes the lines for the operator since the last call: which resource was turned off and why, or on.
    pub fn take_notices(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notices)
    }

    /// Player events dropped so far because a resource that kept running out of budget had too many waiting.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Runs one tick in every resource within `budget`: first each resource's waiting player events, then the
    /// given ones (`joined` and `left` are entity ids), then the `tick` event.
    ///
    /// Each call's deadline is the per-call deadline or what is left of `budget`, whichever is shorter. Once
    /// `budget` is spent, the remaining resources skip this `tick` event and keep their player events for a later
    /// tick. The first resource called moves by one every tick, so no resource is always last. A built-in the
    /// interrupt cannot reach (see [`crate::guards`]) can still run past the budget once.
    pub fn run_tick(
        &mut self,
        tick: u64,
        dt: f64,
        joined: &[u32],
        left: &[u32],
        budget: Duration,
    ) -> TickReport {
        let start = Instant::now();
        let mut report = TickReport::default();
        let n = self.resources.len();
        for k in 0..n {
            let i = (k + (tick % n as u64) as usize) % n;
            let (resource, state) = (&mut self.resources[i], &mut self.state[i]);
            if state.off {
                continue;
            }
            let waiting = &mut state.waiting;
            let events = joined
                .iter()
                .map(|&p| Event::PlayerJoin(p))
                .chain(left.iter().map(|&p| Event::PlayerLeave(p)));
            for event in events.filter(|e| resource.listens(e)) {
                if waiting.len() == MAX_WAITING {
                    waiting.pop_front();
                    self.dropped += 1;
                }
                waiting.push_back(event);
            }
            loop {
                let left_over = budget.saturating_sub(start.elapsed());
                if left_over.is_zero() {
                    if resource.listens(&Event::Tick { tick, dt }) {
                        resource.skip_tick();
                        report.skipped += 1;
                    }
                    break;
                }
                let event = match waiting.pop_front() {
                    Some(e) => e,
                    None => Event::Tick { tick, dt },
                };
                let full = resource.limits().deadline;
                let call = Instant::now();
                let result = resource.dispatch_within(event, left_over);
                let strike = match &result {
                    Err(CallError::Deadline) => left_over >= full,
                    Err(CallError::Memory) => true,
                    _ => call.elapsed() > full,
                };
                if result.is_err() {
                    report.failed += 1;
                }
                if strike {
                    state.strikes += 1;
                } else if !matches!(result, Err(CallError::Deadline)) {
                    state.strikes = 0;
                }
                if self.strike_limit > 0 && state.strikes >= self.strike_limit {
                    state.off = true;
                    state.waiting.clear();
                    let why = match result {
                        Err(e) => e.to_string(),
                        Ok(()) => format!("{:.1?} in one call, past its {full:?} deadline", call.elapsed()),
                    };
                    self.notices.push(format!(
                        "resource {} turned off after {} strikes in a row (last: {why}); `enable {}` turns it back on",
                        resource.name(),
                        state.strikes,
                        resource.name()
                    ));
                    report.turned_off += 1;
                    break;
                }
                if matches!(event, Event::Tick { .. }) {
                    break;
                }
            }
        }
        report.spent = start.elapsed();
        report
    }

    /// The running resources, in call order.
    #[must_use]
    pub fn resources(&self) -> &[Resource] {
        &self.resources
    }

    /// The running resources, to drain their logs.
    pub fn resources_mut(&mut self) -> &mut [Resource] {
        &mut self.resources
    }

    /// Delivers an event to every resource in order and returns the failures by resource name.
    pub fn dispatch(&mut self, event: Event) -> Vec<(&str, CallError)> {
        let mut failed = Vec::new();
        for r in &mut self.resources {
            if let Err(e) = r.dispatch(event) {
                failed.push((r.name(), e));
            }
        }
        failed
    }
}

/// Reads a resource folder: its manifest lists the server scripts, in the order they run.
///
/// ```toml
/// server = ["server/main.luau"]
/// ```
pub fn load_resource(dir: &Path, limits: Limits) -> Result<Resource, LoadError> {
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let fail = |m: String| LoadError::new(&name, m);
    let text = fs::read_to_string(dir.join(MANIFEST)).map_err(|e| fail(format!("{MANIFEST}: {e}")))?;
    let manifest: toml::Table = text.parse().map_err(|e| fail(format!("{MANIFEST}: {e}")))?;
    if let Some(key) = manifest.keys().find(|k| k.as_str() != "server") {
        return Err(fail(format!("{MANIFEST}: unknown key {key:?}")));
    }
    let Some(toml::Value::Array(list)) = manifest.get("server") else {
        return Err(fail(format!("{MANIFEST}: `server` must list the server scripts")));
    };
    let root = dir.canonicalize().map_err(|e| fail(e.to_string()))?;
    let mut sources = Vec::with_capacity(list.len());
    for item in list {
        let Some(path) = item.as_str().filter(|p| check_script_path(p)) else {
            return Err(fail(format!("{MANIFEST}: bad script path {item:?}")));
        };
        // A path inside the folder can still be a link out of it.
        let file = root
            .join(path)
            .canonicalize()
            .map_err(|e| fail(format!("{path}: {e}")))?;
        if !file.starts_with(&root) {
            return Err(fail(format!("{path}: outside the resource folder")));
        }
        let source = fs::read_to_string(&file).map_err(|e| fail(format!("{path}: {e}")))?;
        sources.push((path.to_owned(), source));
    }
    Resource::from_sources(&name, &sources, limits)
}
