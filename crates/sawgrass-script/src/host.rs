//! The resources a server runs, loaded from a folder and called in a fixed order.

use std::fmt;
use std::fs;
use std::path::Path;

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

/// Every running resource, called in name order.
#[derive(Debug, Default)]
pub struct Host {
    resources: Vec<Resource>,
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
                Ok(r) => host.resources.push(r),
                Err(e) => errors.push(e),
            }
        }
        (host, errors)
    }

    /// Adds a resource that is already loaded; it is called after the others.
    pub fn push(&mut self, resource: Resource) {
        self.resources.push(resource);
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
