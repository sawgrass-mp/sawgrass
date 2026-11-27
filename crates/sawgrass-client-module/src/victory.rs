//! The adapter for [VIctory](https://github.com/Marvisak/VIctory), the first runtime (ADR 0004).
//!
//! VIctory loads the module itself: its mod loader (the local `mod-loader` branch, proposed upstream; ABI version
//! [`abi::ABI_VERSION`]) loads each DLL named in `VICTORY_MODS` after the title is mapped and VIctory's own patches
//! are applied, and calls the DLL's [`victory_mod_init`] with a [`abi::Host`]. The module never loads or injects
//! itself, patches the runtime or works around any protection: it runs only when VIctory loads it, and hooks only
//! through the host's `hook`, which refuses bytes VIctory patched itself.
//!
//! What the loader gives, and how the module uses it:
//!
//! - The title's build: its version and the BLAKE3 hash of the original executable, which make the
//!   [`GameBuild`] key a table is written for (`victory/1.0.2.1/<hash in hex>`).
//! - The mapped image, scanned for the table's patterns and `@` offsets. VIctory rewrites code before the module
//!   sees it, so patterns are taken from the running process, not from the file.
//! - `hook` and `unhook`, only while [`victory_mod_init`] runs: every hook is installed at start.
//! - A frame callback at every present, which drives the module's frame, so the table needs no `frame` entry. With
//!   the table's script entries the frame moves to a script thread instead ([`script`]), where natives may run.
//!
//! The tables are files on the player's machine, named by `SAWGRASS_FUNCTION_TABLE` and `SAWGRASS_ASSET_TABLE`.
//! The real [`crate::game::GameApi`] for a build, with its hook detours, arrives once the game's functions are found;
//! until then the module starts, reports in VIctory's log what it found and what is missing, and counts frames.

// The C ABI with VIctory's loader is the only unsafe code in the crate (the task allows it for this adapter).
#![allow(unsafe_code)]

use std::collections::BTreeMap;
use std::ffi::{CStr, c_void};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use crate::assets::AssetTable;
use crate::module::Module;
use crate::runtime::{CodeImage, GameBuild, HookHandle, HookRequest, Runtime, RuntimeError, RuntimeKind};
use crate::table::FunctionTable;

pub mod game;
pub mod natives;
pub mod script;

/// The loader's C ABI, mirrored from VIctory's `victory-mod-api` crate (version 1). It is not a dependency: that
/// crate lives in a local VIctory branch, and this repository takes no git or path dependency outside itself.
pub mod abi {
    use std::ffi::{c_char, c_void};

    /// The ABI version this adapter is written for. A host with another one is refused.
    pub const ABI_VERSION: u32 = 1;

    /// Called once a frame, at the title's present, on the thread that presents.
    pub type FrameFn = unsafe extern "C" fn(user: *mut c_void, frame: u64);

    pub const LOG_ERROR: u32 = 0;
    pub const LOG_WARN: u32 = 1;
    pub const LOG_INFO: u32 = 2;

    /// What VIctory gives a module. New fields only ever go at the end, with `size` growing.
    #[repr(C)]
    #[derive(Debug)]
    pub struct Host {
        pub abi: u32,
        /// `size_of::<Host>()` of the host that filled this in.
        pub size: u32,
        pub title_base: u64,
        /// `SizeOfImage`: the whole mapped image, headers, code and data (where `local_player` lives), readable in
        /// place for this many bytes.
        pub title_size: u64,
        /// BLAKE3 of the original executable.
        pub title_hash: [u8; 32],
        /// NUL-terminated, such as `1.0.2.1`; empty when VIctory does not know the build.
        pub title_version: *const c_char,
        pub log: unsafe extern "C" fn(level: u32, text: *const u8, len: usize),
        /// A handle (> 0), or a [`status_name`] code negated.
        pub hook: unsafe extern "C" fn(target: u64, detour: u64, original: *mut u64) -> i64,
        pub unhook: unsafe extern "C" fn(handle: i64) -> i32,
        pub on_frame: unsafe extern "C" fn(callback: FrameFn, user: *mut c_void) -> i32,
    }

    // The layout of ABI 1 on 64-bit Windows, the loader's only target: drift from the loader's struct shows here.
    #[cfg(target_pointer_width = "64")]
    const _: () = assert!(std::mem::size_of::<Host>() == 96);

    /// The calling thread's block of the title's static TLS: the first field added to ABI 1, right after
    /// [`Host`]'s, present when `size` is at least [`SIZE_WITH_TITLE_TLS`]. It is not a field of [`Host`], since an
    /// older host fills in only the first 96 bytes.
    pub type TitleTlsFn = super::natives::TitleTls;

    /// A host's `size` when it gives `title_tls`.
    pub const SIZE_WITH_TITLE_TLS: usize = std::mem::size_of::<Host>() + std::mem::size_of::<TitleTlsFn>();

    /// The host's `title_tls`, if it is new enough to give one.
    pub fn title_tls(host: &Host) -> Option<TitleTlsFn> {
        if (host.size as usize) < SIZE_WITH_TITLE_TLS {
            return None;
        }
        // SAFETY: a host that says it is this large filled in the field right after the ABI 1 fields, at an
        // 8-aligned offset (96), with the host's function.
        Some(unsafe { std::ptr::from_ref(host).add(1).cast::<TitleTlsFn>().read() })
    }

    /// The name of a status code the host returns (negated, for `hook`).
    pub fn status_name(code: i64) -> &'static str {
        match code.unsigned_abs() {
            0 => "Ok",
            1 => "OutsideImage: the address is not in the title's image",
            2 => "CannotDecode: the function's first bytes could not be decoded or end too early",
            3 => "CannotRelocate: the moved instructions could not be re-encoded",
            4 => "NoMemory: no memory for the trampoline within reach",
            5 => "AlreadyHooked",
            6 => "TooLate: hooks are installed only during init",
            7 => "UnknownHandle",
            8 => "BadArgument",
            9 => "PatchedByVictory: VIctory changed these bytes itself, hook another place",
            _ => "unknown status",
        }
    }
}

const IMAGE_CLOSED: &str = "the image is read only during the start, before the title runs";
const NOT_VICTORY: &str = "no VIctory host, or one with another ABI version";

/// The module's side of a running VIctory.
#[derive(Debug)]
pub struct VictoryRuntime {
    host: &'static abi::Host,
    build: GameBuild,
    base: u64,
    /// `SizeOfImage`. The image is not kept as a slice: it covers data the game writes every frame and code the
    /// hooks rewrite, so a long-lived shared reference to it would be unsound. [`Runtime::code_image`] lends one
    /// only for the start, before any title code runs.
    len: usize,
    /// Whether [`Runtime::code_image`] may still lend the image: until the first hook is installed or the frame
    /// callback registered, which is when the start ends.
    image_open: AtomicBool,
    /// The detour for each hooked function, registered by the build's game layer before the module starts.
    detours: BTreeMap<String, u64>,
    /// Where to call each hooked function's original code.
    originals: BTreeMap<String, u64>,
    /// The host's `title_tls`, which script natives need; `None` from a VIctory that does not give it yet.
    title_tls: Option<abi::TitleTlsFn>,
}

// SAFETY: the runtime holds the host, which stays valid for the whole run (the loader's contract), and two
// integers; it reads the image only through `code_image` during the start. It is used from one thread at a time:
// the module behind it sits in a mutex.
unsafe impl Send for VictoryRuntime {}

impl VictoryRuntime {
    /// Binds to the host VIctory passed to [`victory_mod_init`].
    ///
    /// # Safety
    ///
    /// `host` is null or points to a host VIctory filled in, valid for the rest of the run, whose title image is
    /// readable at `title_base` for `title_size` bytes.
    pub unsafe fn from_host(host: *const abi::Host) -> Result<Self, RuntimeError> {
        // SAFETY: the caller's contract: null or a host valid for the run.
        let host = unsafe { host.as_ref() }.ok_or(RuntimeError::Unavailable(NOT_VICTORY))?;
        if !host_is_usable(host) {
            return Err(RuntimeError::Unavailable(NOT_VICTORY));
        }
        let version = if host.title_version.is_null() {
            String::new()
        } else {
            // SAFETY: a non-null `title_version` is NUL-terminated and valid for the run.
            unsafe { CStr::from_ptr(host.title_version) }
                .to_string_lossy()
                .into_owned()
        };
        let len = usize::try_from(host.title_size).map_err(|_| RuntimeError::Unavailable(NOT_VICTORY))?;
        Ok(Self {
            host,
            build: GameBuild {
                runtime: RuntimeKind::Victory,
                title_version: if version.is_empty() {
                    "unknown".into()
                } else {
                    version
                },
                image_digest: hex(&host.title_hash),
            },
            base: host.title_base,
            len,
            image_open: AtomicBool::new(true),
            detours: BTreeMap::new(),
            originals: BTreeMap::new(),
            title_tls: abi::title_tls(host),
        })
    }

    /// Registers the detour for a function, before [`Module::start`] hooks it. A hook without one is refused, and
    /// its events stay off.
    pub fn set_detour(&mut self, function: &str, detour: u64) {
        self.detours.insert(function.to_owned(), detour);
    }

    /// Where a hooked function's original code is called, for its detour.
    pub fn original(&self, function: &str) -> Option<u64> {
        self.originals.get(function).copied()
    }

    /// The host's `title_tls`, for [`natives::ScriptScope::enter`]; `None` when this VIctory cannot run natives.
    pub fn title_tls(&self) -> Option<abi::TitleTlsFn> {
        self.title_tls
    }

    /// Writes a line to VIctory's log.
    pub fn log(&self, level: u32, text: &str) {
        // SAFETY: `log` takes UTF-8 bytes and their length, which `text` is.
        unsafe { (self.host.log)(level, text.as_ptr(), text.len()) };
    }

    /// Hooks `request` with `detour`, the host writing the original's address straight into `original`, so that it
    /// is there before the hook goes live on another thread.
    fn hook_into(
        &mut self,
        request: HookRequest,
        detour: u64,
        original: &'static AtomicU64,
    ) -> Result<HookHandle, RuntimeError> {
        self.image_open.store(false, Ordering::Relaxed);
        // SAFETY: `original` is a valid place for the host to write the trampoline's address.
        let result = unsafe { (self.host.hook)(request.address, detour, original.as_ptr()) };
        if result > 0 {
            Ok(HookHandle(result as u64))
        } else {
            Err(RuntimeError::HookRefused {
                function: request.function,
                reason: abi::status_name(result).into(),
            })
        }
    }

    /// Has the host call `callback` at every present. The callback finds the module in a static, so no user
    /// pointer is passed.
    fn on_frame(&self, callback: abi::FrameFn) -> Result<(), RuntimeError> {
        self.image_open.store(false, Ordering::Relaxed);
        // SAFETY: the host takes any callback of this type, and hands the null user pointer back untouched.
        let status = unsafe { (self.host.on_frame)(callback, std::ptr::null_mut()) };
        if status == 0 {
            Ok(())
        } else {
            Err(RuntimeError::HookRefused {
                function: "frame callback".into(),
                reason: abi::status_name(i64::from(status)).into(),
            })
        }
    }
}

/// Whether the host is one this adapter can read: the right ABI version, and at least the fields it knows of.
fn host_is_usable(host: &abi::Host) -> bool {
    host.abi == abi::ABI_VERSION && host.size as usize >= std::mem::size_of::<abi::Host>()
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

impl Runtime for VictoryRuntime {
    fn kind(&self) -> RuntimeKind {
        RuntimeKind::Victory
    }

    fn build(&self) -> Result<GameBuild, RuntimeError> {
        Ok(self.build.clone())
    }

    fn code_image(&self) -> Result<CodeImage<'_>, RuntimeError> {
        if !self.image_open.load(Ordering::Relaxed) {
            return Err(RuntimeError::Unavailable(IMAGE_CLOSED));
        }
        // SAFETY: the loader maps the image readable for `len` bytes at `base` for the run. It is lent only until
        // the first hook or the frame callback, that is while `Module::start` resolves the table inside
        // `victory_mod_init`, before any title code runs, so nothing writes the image while it is borrowed.
        let bytes = unsafe { std::slice::from_raw_parts(self.base as *const u8, self.len) };
        Ok(CodeImage {
            base: self.base,
            bytes,
        })
    }

    fn install_hook(&mut self, request: HookRequest) -> Result<HookHandle, RuntimeError> {
        self.image_open.store(false, Ordering::Relaxed);
        let refused = |reason: &str| RuntimeError::HookRefused {
            function: request.function.clone(),
            reason: reason.into(),
        };
        let detour = *self
            .detours
            .get(&request.function)
            .ok_or_else(|| refused("no detour written for it in this build's game layer yet"))?;
        let mut original = 0u64;
        // SAFETY: `original` is a valid place for the host to write the trampoline's address.
        let result = unsafe { (self.host.hook)(request.address, detour, &mut original) };
        if result > 0 {
            self.originals.insert(request.function.clone(), original);
            Ok(HookHandle(result as u64))
        } else {
            Err(refused(abi::status_name(result)))
        }
    }

    fn remove_hook(&mut self, handle: HookHandle) -> Result<(), RuntimeError> {
        let raw = i64::try_from(handle.0).map_err(|_| RuntimeError::UnknownHook(handle))?;
        // SAFETY: the host checks the handle itself and answers UnknownHandle for one it does not know.
        let status = unsafe { (self.host.unhook)(raw) };
        if status == 0 {
            Ok(())
        } else {
            Err(RuntimeError::UnknownHook(handle))
        }
    }

    fn drives_frames(&self) -> bool {
        true
    }
}

/// The started module, for the frame callback.
#[derive(Debug)]
struct Running {
    module: Module<VictoryRuntime>,
    frames: u64,
    /// Set when the frame runs on a script thread.
    script: Option<script::Script>,
}

static RUNNING: Mutex<Option<Running>> = Mutex::new(None);

/// How often the frame callback reports that it runs, in frames.
const FRAME_REPORT: u64 = 600;

/// Starts the module on `runtime` with the two tables' text, and registers the frame callback. Everything it found
/// and everything missing goes to VIctory's log.
fn start(runtime: VictoryRuntime, functions: &str, assets: &str) -> Result<(), String> {
    let key = runtime.build.key();
    runtime.log(abi::LOG_INFO, &format!("sawgrass: running build {key}"));
    let functions = FunctionTable::parse(functions).map_err(|e| format!("function table: {e}"))?;
    let assets = AssetTable::parse(assets).map_err(|e| format!("asset table: {e:?}"))?;
    // The script entries are read and their natives checked first, while the image is lent.
    let prepared = script::prepare(&runtime, &functions);
    let mut module = match Module::start(runtime, &functions, assets) {
        Ok(module) => module,
        Err((error, runtime)) => {
            let text = format!("sawgrass: not started: {error}");
            runtime.log(abi::LOG_ERROR, &text);
            return Err(text);
        }
    };
    let runtime = module.runtime();
    runtime.log(
        abi::LOG_INFO,
        &format!(
            "sawgrass: started, {} functions located",
            module.functions().addresses.len()
        ),
    );
    if !module.moves_entities() {
        runtime.log(
            abi::LOG_WARN,
            "sawgrass: entity_set_pose not located: no entity is moved, and the server cannot correct the player",
        );
    }
    if !module.shows_remote_peds() {
        runtime.log(
            abi::LOG_WARN,
            "sawgrass: ped_spawn not located: the local player is sent, but remote players are not shown yet",
        );
    }
    if runtime.title_tls().is_none() {
        runtime.log(
            abi::LOG_WARN,
            "sawgrass: this VIctory gives no title_tls, so script natives cannot be called",
        );
    }
    for (name, why) in module.missing_functions() {
        runtime.log(
            abi::LOG_WARN,
            &format!("sawgrass: off until found: {name} ({why:?})"),
        );
    }
    for (point, why) in module.refused_hooks() {
        runtime.log(
            abi::LOG_WARN,
            &format!("sawgrass: hook {} off: {why}", point.function()),
        );
    }
    let script = prepared.and_then(|prepared| script::install(module.runtime_mut(), prepared));
    module.runtime().on_frame(frame).map_err(|e| e.to_string())?;
    *RUNNING.lock().unwrap_or_else(PoisonError::into_inner) = Some(Running {
        module,
        frames: 0,
        script,
    });
    Ok(())
}

/// The frame callback.
///
/// TODO(game layer): run `Module::frame` with the build's `GameApi` and the network core once the game's functions
/// are found; until then it counts frames, which shows in the log that the module runs with the game.
unsafe extern "C" fn frame(_user: *mut c_void, _frame: u64) {
    script::PRESENTS.fetch_add(1, Ordering::Relaxed);
    if script::ON.load(Ordering::Acquire) {
        return;
    }
    let _ = std::panic::catch_unwind(|| {
        let mut running = RUNNING.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(running) = running.as_mut() {
            running.frames += 1;
            if running.frames % FRAME_REPORT == 0 {
                let text = format!("sawgrass: {} frames", running.frames);
                running.module.runtime().log(abi::LOG_INFO, &text);
            }
        }
    });
}

fn read_table(variable: &str) -> Result<String, String> {
    let path = std::env::var_os(variable).ok_or_else(|| format!("{variable} is not set"))?;
    std::fs::read_to_string(&path).map_err(|e| format!("{variable} ({}): {e}", path.to_string_lossy()))
}

/// The entry point VIctory's loader calls: `0` if the module started, `1` if not, with nothing installed.
///
/// # Safety
///
/// Called by VIctory with a host valid for the rest of the run, once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn victory_mod_init(host: *const abi::Host) -> i32 {
    // SAFETY: VIctory passes its host, valid for the run.
    let runtime = match unsafe { VictoryRuntime::from_host(host) } {
        Ok(runtime) => runtime,
        Err(_) => return 1,
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let tables =
            read_table("SAWGRASS_FUNCTION_TABLE").and_then(|f| Ok((f, read_table("SAWGRASS_ASSET_TABLE")?)));
        match tables {
            Ok((functions, assets)) => start(runtime, &functions, &assets),
            Err(e) => {
                runtime.log(abi::LOG_ERROR, &format!("sawgrass: not started: {e}"));
                Err(e)
            }
        }
    }));
    match result {
        Ok(Ok(())) => 0,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::c_char;
    use std::sync::atomic::{AtomicI64, AtomicUsize};

    /// What the test host was asked, in order.
    static CALLS: Mutex<Vec<String>> = Mutex::new(Vec::new());
    /// What the test host's `hook` answers.
    static HOOK_ANSWER: AtomicI64 = AtomicI64::new(7);
    static FRAME_CALLBACKS: AtomicUsize = AtomicUsize::new(0);
    static CALLBACK: Mutex<Option<abi::FrameFn>> = Mutex::new(None);

    fn calls() -> Vec<String> {
        std::mem::take(&mut *CALLS.lock().unwrap_or_else(PoisonError::into_inner))
    }

    fn record(call: String) {
        CALLS.lock().unwrap_or_else(PoisonError::into_inner).push(call);
    }

    unsafe extern "C" fn log(level: u32, text: *const u8, len: usize) {
        // SAFETY: the adapter passes UTF-8 bytes and their length.
        let text = unsafe { std::slice::from_raw_parts(text, len) };
        record(format!("log {level} {}", String::from_utf8_lossy(text)));
    }

    unsafe extern "C" fn hook(target: u64, detour: u64, original: *mut u64) -> i64 {
        record(format!("hook {target:#x} -> {detour:#x}"));
        // SAFETY: the adapter passes a valid place.
        unsafe { *original = fake_run as *const () as u64 };
        HOOK_ANSWER.load(Ordering::Relaxed)
    }

    unsafe extern "C" fn unhook(handle: i64) -> i32 {
        record(format!("unhook {handle}"));
        0
    }

    unsafe extern "C" fn on_frame(callback: abi::FrameFn, _user: *mut c_void) -> i32 {
        FRAME_CALLBACKS.fetch_add(1, Ordering::Relaxed);
        *CALLBACK.lock().unwrap_or_else(PoisonError::into_inner) = Some(callback);
        0
    }

    const VERSION: &CStr = c"1.0.2.1";

    /// A host over a made-up image of 0x100 bytes, kept for the rest of the test run.
    fn host(abi_version: u32) -> &'static abi::Host {
        let image: &'static mut [u8] = Box::leak(vec![0u8; 0x100].into_boxed_slice());
        image[0x40..0x44].copy_from_slice(&[0x5A, 0xA5, 0x3C, 0xC3]);
        let mut hash = [0u8; 32];
        hash[0] = 0xAB;
        hash[31] = 0x01;
        Box::leak(Box::new(abi::Host {
            abi: abi_version,
            size: std::mem::size_of::<abi::Host>() as u32,
            title_base: image.as_ptr() as u64,
            title_size: image.len() as u64,
            title_hash: hash,
            title_version: VERSION.as_ptr().cast::<c_char>(),
            log,
            hook,
            unhook,
            on_frame,
        }))
    }

    unsafe extern "C" fn test_tls() -> *mut c_void {
        std::ptr::dangling_mut()
    }

    /// A host that gives `title_tls`.
    #[repr(C)]
    struct Newer {
        host: abi::Host,
        title_tls: abi::TitleTlsFn,
    }

    /// The title's TLS block for the script frame test; the current context is at 0x10.
    static mut BLOCK: [u64; 4] = [0; 4];
    static RUNS: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn block_tls() -> *mut c_void {
        std::ptr::addr_of_mut!(BLOCK).cast()
    }

    /// The script run function's original code, as the test host hands it back for every hook.
    unsafe extern "C" fn fake_run(_: u64, _: u64, _: u64, _: u64, _: u64, _: u64) -> u64 {
        RUNS.fetch_add(1, Ordering::Relaxed);
        7
    }

    fn newer_host(old: &abi::Host, title_tls: abi::TitleTlsFn) -> &'static abi::Host {
        // SAFETY: a copy of the old host's fields, which are plain values and functions.
        let mut newer = Newer {
            host: unsafe { std::ptr::read(old) },
            title_tls,
        };
        newer.host.size = abi::SIZE_WITH_TITLE_TLS as u32;
        let newer: &'static Newer = Box::leak(Box::new(newer));
        // SAFETY: `Host` is the first field of the `repr(C)` `Newer`, and the pointer keeps the whole of it.
        unsafe { &*std::ptr::from_ref(newer).cast::<abi::Host>() }
    }

    #[test]
    fn title_tls_is_read_only_from_a_host_that_gives_it() {
        assert_eq!(abi::SIZE_WITH_TITLE_TLS, std::mem::size_of::<Newer>());
        let old = host(abi::ABI_VERSION);
        assert!(abi::title_tls(old).is_none());
        let tls = abi::title_tls(newer_host(old, test_tls)).expect("given");
        // SAFETY: the test's function.
        assert_eq!(unsafe { tls() }, std::ptr::dangling_mut());
    }

    #[test]
    fn refuses_no_host_or_another_abi() {
        // SAFETY: null, or a host built above.
        unsafe {
            assert!(VictoryRuntime::from_host(std::ptr::null()).is_err());
            assert!(VictoryRuntime::from_host(host(2)).is_err());
            assert_eq!(victory_mod_init(std::ptr::null()), 1);
        }
    }

    /// One test drives the test host, since it records into statics shared by the whole run.
    #[test]
    fn reads_the_build_hooks_through_the_host_and_starts() {
        let host = host(abi::ABI_VERSION);
        // SAFETY: a host built above, valid for the run.
        let mut runtime = unsafe { VictoryRuntime::from_host(host) }.expect("runtime");
        let build = runtime.build().expect("build");
        let digest = format!("ab{}01", "00".repeat(30));
        assert_eq!(build.key(), format!("victory/1.0.2.1/{digest}"));
        let image = runtime.code_image().expect("image");
        assert_eq!((image.base, image.bytes.len()), (host.title_base, 0x100));
        assert_eq!(image.bytes[0x40..0x44], [0x5A, 0xA5, 0x3C, 0xC3]);
        assert!(runtime.drives_frames());

        // A hook without a detour is refused before the host is asked; one with a detour goes to the host.
        let request = |function: &str| HookRequest {
            function: function.into(),
            address: host.title_base + 0x40,
        };
        assert!(matches!(
            runtime.install_hook(request("ped_create")),
            Err(RuntimeError::HookRefused { .. })
        ));
        assert!(calls().is_empty());
        runtime.set_detour("ped_create", 0xD000);
        let handle = runtime.install_hook(request("ped_create")).expect("hooked");
        assert_eq!(handle, HookHandle(7));
        assert_eq!(runtime.original("ped_create"), Some(fake_run as *const () as u64));
        HOOK_ANSWER.store(-9, Ordering::Relaxed);
        let Err(RuntimeError::HookRefused { reason, .. }) = runtime.install_hook(request("ped_create"))
        else {
            panic!("refused");
        };
        assert!(reason.starts_with("PatchedByVictory"), "{reason}");
        runtime.remove_hook(handle).expect("removed");
        // A hook ends the start: the image, which the game writes once it runs, is no longer lent.
        assert_eq!(
            runtime.code_image().err(),
            Some(RuntimeError::Unavailable(IMAGE_CLOSED))
        );
        let base = host.title_base;
        assert_eq!(
            calls(),
            vec![
                format!("hook {:#x} -> 0xd000", base + 0x40),
                format!("hook {:#x} -> 0xd000", base + 0x40),
                "unhook 7".to_owned(),
            ]
        );

        // Started with three entries, as `@` offsets: no frame entry, since VIctory calls the frame.
        let key = build.key();
        let functions = format!(
            "build = {key}\nlocal_player = @0x80\nped_spawn = @0x40\nentity_set_pose = 5A A5 3C C3 ; offset=1\n"
        );
        let assets = format!("build = {key}\nped = 0x2000\n");
        // SAFETY: the same host, as VIctory would pass it to a fresh start.
        let runtime = unsafe { VictoryRuntime::from_host(host) }.expect("runtime");
        start(runtime, &functions, &assets).expect("started");
        assert_eq!(FRAME_CALLBACKS.load(Ordering::Relaxed), 1);
        let log = calls();
        assert_eq!(log[0], format!("log 2 sawgrass: running build {key}"));
        assert_eq!(log[1], "log 2 sawgrass: started, 3 functions located");
        assert!(!log.iter().any(|l| l.contains("ped_spawn not located")));
        assert!(log.iter().any(|l| l.contains("off until found: ped_create")));
        let callback = CALLBACK
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .expect("callback");
        for frame in 0..FRAME_REPORT {
            // SAFETY: as the host calls it.
            unsafe { callback(std::ptr::null_mut(), frame) };
        }
        assert_eq!(calls(), vec![format!("log 2 sawgrass: {FRAME_REPORT} frames")]);

        // With the script run function and the layout in the table, the frame moves to a script thread: once per
        // present, at the first run on a thread the title is updating, with that run's context current.
        let script = "script_run = @0x10\nentity_position = @0x20 ; hash=0x1\n\
                      value script_tls_context = 0x10\nvalue script_context_thread = 0x8\n\
                      value script_thread_state = 0x4\nvalue script_thread_updating = 0xC\n";
        HOOK_ANSWER.store(8, Ordering::Relaxed);
        // SAFETY: a host built above, now giving `title_tls`.
        let runtime = unsafe { VictoryRuntime::from_host(newer_host(host, block_tls)) }.expect("runtime");
        start(runtime, &format!("{functions}{script}"), &assets).expect("started");
        let log = calls();
        assert!(
            log.iter()
                .any(|l| l.contains("native entity_position refused: no `native_guard`")),
            "{log:?}"
        );
        assert!(
            log.iter().any(|l| l.ends_with("frame runs on a script thread")),
            "{log:?}"
        );
        let thread: &'static mut [u8; 8] = Box::leak(Box::new([0; 8]));
        let context: &'static [u64; 2] = Box::leak(Box::new([0, std::ptr::from_mut(thread) as u64]));
        let context = context.as_ptr() as u64;
        let run = |context| {
            // SAFETY: as the title calls it, with a context laid out as the table says.
            assert_eq!(unsafe { script::script_run(context, 0, 0, 0, 0, 0) }, 7);
        };
        // SAFETY: the test's thread, read by the frame through the context.
        unsafe { std::ptr::from_mut(thread).cast::<u8>().add(4).write(2) };
        RUNS.store(0, Ordering::Relaxed);
        for frame in 0..FRAME_REPORT {
            // SAFETY: as the host calls it.
            unsafe { callback(std::ptr::null_mut(), frame) };
            run(0);
            run(context);
            run(context);
        }
        assert_eq!(RUNS.load(Ordering::Relaxed), 3 * FRAME_REPORT as usize);
        assert_eq!(
            calls(),
            vec![format!(
                "log 2 sawgrass: {FRAME_REPORT} frames on a script thread"
            )]
        );
        // SAFETY: the test's block, after every scope is gone.
        assert_eq!(unsafe { (*std::ptr::addr_of!(BLOCK))[2] }, 0);
    }
}
