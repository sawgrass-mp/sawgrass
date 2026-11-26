//! The boundary between the module and the runtime that runs the game (ADR 0004).
//!
//! A [`Runtime`] adapter is the only code that knows which runtime it serves: how the module was loaded, where the
//! title's code is, which build it is, and how a hook is installed. Everything else in the module talks to this
//! trait, so it is tested with [`crate::fake::FakeRuntime`] and needs no game.

use std::fmt;

/// The runtimes ADR 0004 supports. XCL waits for the licence question in that ADR.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuntimeKind {
    Victory,
    Xcl,
}

impl RuntimeKind {
    /// The name used in build keys and function tables.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Victory => "victory",
            Self::Xcl => "xcl",
        }
    }
}

/// Which build of the game is running, under which runtime.
///
/// Function locations move with every game patch and every runtime that rebuilds the code, so a function table is
/// written for one `GameBuild` and refused for any other ([`crate::table`]).
///
/// ASSUMPTION: the runtime can tell us the title's version string and a digest of its code image. Neither VIctory
/// nor XCL has an API for this yet; the upstream proposal in ADR 0004 asks for "build identity". Until then the
/// adapter computes the digest itself over the code it is given.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GameBuild {
    pub runtime: RuntimeKind,
    /// The title's version as the runtime reports it, for people to read.
    pub title_version: String,
    /// A digest of the title's code image, in lowercase hex. This is what tells two builds apart.
    pub image_digest: String,
}

impl GameBuild {
    /// `runtime/title_version/image_digest`, the key a function table names in its `build` line.
    pub fn key(&self) -> String {
        format!(
            "{}/{}/{}",
            self.runtime.as_str(),
            self.title_version,
            self.image_digest
        )
    }
}

/// The title's image as the runtime has it mapped: its code, and the data around it that table entries such as
/// `local_player` may name.
#[derive(Debug, Clone, Copy)]
pub struct CodeImage<'a> {
    /// Address of `bytes[0]` in the game process.
    pub base: u64,
    pub bytes: &'a [u8],
}

/// Identifies an installed hook so that it can be removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HookHandle(pub u64);

/// A request to the runtime to divert one game function to the module.
///
/// ASSUMPTION: the runtime installs the detour and calls the module's handler for `function` with the original
/// available, as the "register hooks by build identity" part of the upstream proposal describes. How arguments are
/// passed is the adapter's business and arrives with the real hooks (CLIENT-2 defines what each hook reports).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookRequest {
    /// The function table name of the hooked function, such as `ped_create`.
    pub function: String,
    pub address: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeError {
    /// The runtime does not offer what the module needs yet.
    Unavailable(&'static str),
    /// The runtime refused a hook.
    HookRefused {
        function: String,
        reason: String,
    },
    UnknownHook(HookHandle),
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(what) => write!(f, "runtime does not support this yet: {what}"),
            Self::HookRefused { function, reason } => write!(f, "hook on `{function}` refused: {reason}"),
            Self::UnknownHook(handle) => write!(f, "no hook with handle {}", handle.0),
        }
    }
}

impl std::error::Error for RuntimeError {}

/// What the module needs from the runtime that loaded it.
pub trait Runtime {
    fn kind(&self) -> RuntimeKind;
    fn build(&self) -> Result<GameBuild, RuntimeError>;
    fn code_image(&self) -> Result<CodeImage<'_>, RuntimeError>;
    fn install_hook(&mut self, request: HookRequest) -> Result<HookHandle, RuntimeError>;
    fn remove_hook(&mut self, handle: HookHandle) -> Result<(), RuntimeError>;
    /// Whether the runtime calls the module once a frame by itself, as VIctory's frame callback does. Then the
    /// module needs no `frame` entry in its table and hooks no frame function.
    fn drives_frames(&self) -> bool {
        false
    }
}
