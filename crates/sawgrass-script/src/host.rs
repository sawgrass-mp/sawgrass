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

/// Every running resource, called in name order.
#[derive(Debug, Default)]
pub struct Host {
    resources: Vec<Resource>,
    /// Player events each resource has not had yet, by resource index.
    waiting: Vec<VecDeque<Event>>,
    /// Player events dropped because a resource's queue was full.
    dropped: u64,
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
        self.waiting.push(VecDeque::new());
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
            let (resource, waiting) = (&mut self.resources[i], &mut self.waiting[i]);
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
                if resource.dispatch_within(event, left_over).is_err() {
                    report.failed += 1;
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
