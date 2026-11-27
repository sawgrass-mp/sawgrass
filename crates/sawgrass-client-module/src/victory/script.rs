//! The module's frame on one of the title's script threads, where script natives may be called.
//!
//! VIctory's frame callback runs at the title's present, which is not a script thread. When the table gives the
//! title's script run function and the script-thread layout, the adapter hooks that function: after the title has run
//! a script, the first time in each presented frame that the thread is one the title is updating, the context it just
//! ran is made current ([`ScriptScope`]) and the module's frame runs there. Without those entries, or without the
//! host's `title_tls`, the frame stays at the present and natives are not called.

use std::fmt::Write as _;
use std::sync::TryLockError;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::game::{ENTITY_HEADING, GameNatives, Handles, NativeGame};
use super::natives::{Native, NativeCheck, ScriptLayout, ScriptScope, TitleTls};
use super::{FRAME_REPORT, Net, RUNNING, VictoryRuntime, abi};
use crate::connection::Status;
use crate::game::GameApi;
use crate::module::ENTITY_SET_POSE;
use crate::module::Module;
use crate::runtime::{HookRequest, Runtime};
use crate::table::FunctionTable;
use std::time::Instant;

/// Table names: the script run function (six integer arguments, the context first), and the layout values.
pub const SCRIPT_RUN: &str = "script_run";
pub const TLS_CONTEXT: &str = "script_tls_context";
pub const CONTEXT_THREAD: &str = "script_context_thread";
pub const THREAD_STATE: &str = "script_thread_state";
pub const THREAD_UPDATING: &str = "script_thread_updating";
/// Natives the game layer calls ([`super::game`]), when the table has them with a `hash=`: the local player's ped
/// and an entity's position (which the frame report reads), its heading, and the setter of its position, which is
/// the module's `entity_set_pose`.
pub const LOCAL_PLAYER: &str = "local_player";
pub const ENTITY_POSITION: &str = "entity_position";

/// Presents so far, counted by the frame callback.
pub(super) static PRESENTS: AtomicU64 = AtomicU64::new(0);
/// The present the last script frame ran in.
static LAST: AtomicU64 = AtomicU64::new(0);
/// Whether the frame runs on a script thread, so that the present only counts.
pub(super) static ON: AtomicBool = AtomicBool::new(false);
/// The script run function's original code, written by the host before its hook goes live.
static ORIGINAL: AtomicU64 = AtomicU64::new(0);

type Run = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64) -> u64;

/// What the script frame needs.
#[derive(Debug)]
pub(super) struct Script {
    layout: ScriptLayout,
    title_tls: TitleTls,
    natives: GameNatives,
    handles: Handles,
}

/// The script run function's address and the frame's needs, found while the image is still lent.
#[derive(Debug)]
pub(super) struct Prepared {
    run: u64,
    script: Script,
}

/// Reads the table's script entries and checks its natives. `None`, with the reason in the log, leaves the frame at
/// the present; a table with no `script_run` says nothing, as before.
pub(super) fn prepare(runtime: &VictoryRuntime, table: &FunctionTable) -> Option<Prepared> {
    if !table.entries.contains_key(SCRIPT_RUN) {
        return None;
    }
    let off = |why: &str| {
        runtime.log(
            abi::LOG_WARN,
            &format!("sawgrass: frame stays at the present: {why}"),
        );
    };
    let Some(title_tls) = runtime.title_tls() else {
        off("this VIctory gives no title_tls");
        return None;
    };
    let names = [TLS_CONTEXT, CONTEXT_THREAD, THREAD_STATE, THREAD_UPDATING];
    let missing: Vec<&str> = names
        .into_iter()
        .filter(|name| !table.values.contains_key(*name))
        .collect();
    if !missing.is_empty() {
        off(&format!("no value for {}", missing.join(", ")));
        return None;
    }
    let offset = |name: &str| usize::try_from(table.values[name]).ok();
    let (Some(tls_context), Some(context_thread), Some(thread_state)) =
        (offset(TLS_CONTEXT), offset(CONTEXT_THREAD), offset(THREAD_STATE))
    else {
        off("a layout value is too large");
        return None;
    };
    let layout = ScriptLayout {
        tls_context,
        context_thread,
        thread_state,
        updating: table.values[THREAD_UPDATING],
    };
    let (build, image) = match (runtime.build(), runtime.code_image()) {
        (Ok(build), Ok(image)) => (build, image),
        (Err(e), _) | (_, Err(e)) => {
            off(&e.to_string());
            return None;
        }
    };
    let resolved = match table.resolve(
        &build,
        image,
        &[SCRIPT_RUN],
        &[LOCAL_PLAYER, ENTITY_POSITION, ENTITY_HEADING, ENTITY_SET_POSE],
    ) {
        Ok(resolved) => resolved,
        Err(e) => {
            off(&e.to_string());
            return None;
        }
    };
    let check = NativeCheck::from_table(table);
    let native = |name: &str| -> Option<Native> {
        let address = resolved.address(name)?;
        let hash = table.entries[name].hash?;
        let result = check
            .as_ref()
            .map_err(Clone::clone)
            .and_then(|check| Native::verified(&image, address, hash, check).map_err(|e| e.to_string()));
        match result {
            Ok(native) => {
                runtime.log(abi::LOG_INFO, &format!("sawgrass: native {name} checked"));
                Some(native)
            }
            Err(why) => {
                runtime.log(abi::LOG_WARN, &format!("sawgrass: native {name} refused: {why}"));
                None
            }
        }
    };
    let script = Script {
        layout,
        title_tls,
        natives: GameNatives {
            local_player: native(LOCAL_PLAYER),
            entity_position: native(ENTITY_POSITION),
            entity_heading: native(ENTITY_HEADING),
            entity_set_pose: native(ENTITY_SET_POSE),
        },
        handles: Handles::default(),
    };
    Some(Prepared {
        run: resolved.address(SCRIPT_RUN)?,
        script,
    })
}

/// Hooks the script run function. `None`, with the reason in the log, leaves the frame at the present.
pub(super) fn install(runtime: &mut VictoryRuntime, prepared: Prepared) -> Option<Script> {
    let request = HookRequest {
        function: SCRIPT_RUN.into(),
        address: prepared.run,
    };
    match runtime.hook_into(request, script_run as *const () as u64, &ORIGINAL) {
        Ok(_) => {
            runtime.log(abi::LOG_INFO, "sawgrass: frame runs on a script thread");
            ON.store(true, Ordering::Release);
            Some(prepared.script)
        }
        Err(e) => {
            runtime.log(
                abi::LOG_WARN,
                &format!("sawgrass: frame stays at the present: {e}"),
            );
            None
        }
    }
}

/// The detour on the script run function.
pub(super) unsafe extern "C" fn script_run(a: u64, b: u64, c: u64, d: u64, e: u64, f: u64) -> u64 {
    // SAFETY: the host writes the trampoline before the hook goes live; `Option` of a function pointer has the
    // null niche, so a zero is `None` rather than an invalid pointer.
    let original =
        unsafe { std::mem::transmute::<usize, Option<Run>>(ORIGINAL.load(Ordering::Acquire) as usize) };
    let Some(original) = original else {
        return 0;
    };
    // SAFETY: the original function, with the arguments the title gave.
    let result = unsafe { original(a, b, c, d, e, f) };
    let presents = PRESENTS.load(Ordering::Relaxed);
    if presents != LAST.load(Ordering::Relaxed) {
        // SAFETY: `a` is the context the title just ran on this thread.
        let _ = std::panic::catch_unwind(|| unsafe { frame(a, presents) });
    }
    result
}

/// One script frame, if `context` is on a thread the title is updating.
///
/// # Safety
///
/// `context` is the script context the title just ran on this thread.
unsafe fn frame(context: u64, presents: u64) {
    let mut guard = match RUNNING.try_lock() {
        Ok(guard) => guard,
        Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        // Another thread has the module: never block a title thread.
        Err(TryLockError::WouldBlock) => return,
    };
    let Some(running) = guard.as_mut() else {
        return;
    };
    let Some(script) = running.script.as_ref() else {
        return;
    };
    // SAFETY: the caller's contract, with the host's `title_tls` and the table's layout for this build.
    let Some(scope) = (unsafe { ScriptScope::enter(context, &script.layout, script.title_tls) }) else {
        return;
    };
    LAST.store(presents, Ordering::Relaxed);
    running.frames += 1;
    if let Some(net) = running.net.as_mut() {
        network_frame(net, &mut running.module, &scope, script);
    }
    if running.frames % FRAME_REPORT == 0 {
        let mut text = format!("sawgrass: {} frames on a script thread", running.frames);
        if let Some([x, y, z]) = local_position(&scope, script) {
            let _ = write!(text, ", local player at ({x:.2}, {y:.2}, {z:.2})");
        } else if script.natives.local_player.is_some() {
            text.push_str(", local player not read");
        }
        if let Some(net) = running.net.as_ref() {
            match net.link.client() {
                Some(client) => {
                    let _ = write!(
                        text,
                        "; joined as entity {}, {} shown, {} not shown yet",
                        client.entity(),
                        running.module.entities().len(),
                        net.link.not_located
                    );
                }
                None => text.push_str("; not joined yet"),
            }
        }
        running.module.runtime().log(abi::LOG_INFO, &text);
    }
}

/// One frame of the network core: the module's frame over the natives, fed to the link, and each change of the
/// connection's status logged once.
fn network_frame(net: &mut Net, module: &mut Module<VictoryRuntime>, scope: &ScriptScope, script: &Script) {
    let now = Instant::now();
    // The first frame, and one after a long stall such as a load, count as a 60th of a second: the network core
    // smooths over time and would otherwise take a stall as one long step.
    let dt = net
        .last
        .map(|last| now.duration_since(last).as_secs_f32())
        .filter(|dt| *dt < MAX_DT)
        .unwrap_or(1.0 / 60.0);
    net.last = Some(now);
    let mut game = NativeGame::new(scope, &script.natives, &script.handles);
    if let Err(e) = module.frame(dt, &mut game, &mut net.link) {
        net.frame_errors += 1;
        if net.frame_errors == 1 || net.frame_errors % FRAME_REPORT == 0 {
            let text = format!("sawgrass: frame failed ({} times): {e}", net.frame_errors);
            module.runtime().log(abi::LOG_WARN, &text);
        }
    }
    let status = net.connection.status();
    if status != net.status {
        let text = match &status {
            Status::Connecting => "sawgrass: connecting".to_owned(),
            Status::Connected => "sawgrass: connected".to_owned(),
            Status::Closed(why) => format!("sawgrass: connection closed: {why}"),
        };
        let level = if matches!(status, Status::Closed(_)) {
            abi::LOG_WARN
        } else {
            abi::LOG_INFO
        };
        module.runtime().log(level, &text);
        net.status = status;
    }
}

/// The longest frame the network core takes as it is, in seconds.
const MAX_DT: f32 = 0.25;

/// The local player's position, through the game layer's natives, when the table has them.
fn local_position(scope: &ScriptScope, script: &Script) -> Option<[f32; 3]> {
    let game = NativeGame::new(scope, &script.natives, &script.handles);
    Some(game.pose(game.local_player()?)?.position)
}
