//! Calls into the title's script natives, the way the title's own scripts call them.
//!
//! A native is a handler function the title runs for one script command: `handler(&mut Call)`, with the arguments
//! and the return values in arrays of 64-bit slots. A handler runs only on a script thread the title is updating,
//! with that thread's execution context current in the title's thread-local storage, so a call goes through a
//! [`ScriptScope`]: entered right after the title ran a script context on this thread, it makes that context current,
//! and puts back what was there when it is dropped.
//!
//! Nothing about the title is built in. Every address, offset and hash comes from the function table on the
//! player's machine, and so does the way a handler is checked before it is used ([`Native::verified`]): the table
//! gives guard patterns to find in the handler's first bytes, the relative reference each one leads through, and how
//! a native's hash turns into the 64-bit identity the code there must hold. A wrong address in a table is refused
//! instead of called, and the check never runs the handler.

use std::ffi::c_void;

use crate::pattern::Pattern;
use crate::runtime::CodeImage;
use crate::table::{Entry, FunctionTable, Locator, follow};

/// What a handler is given.
#[repr(C)]
#[derive(Debug)]
pub struct Call {
    pub args: *mut u64,
    pub ret: *mut u64,
    pub argc: i32,
    pub retc: u32,
}

/// A native handler.
pub type Handler = unsafe extern "C" fn(*mut Call);

/// Table names for the check: guard entries (`native_guard`, `native_guard_<anything>`), and its values.
pub const GUARD: &str = "native_guard";
pub const CHECK_WINDOW: &str = "native_check_window";
pub const IDENTITY_WINDOW: &str = "native_identity_window";
pub const IDENTITY_ROTATE: &str = "native_identity_rotate";
pub const IDENTITY_XOR: &str = "native_identity_xor";

/// Why a table's handler is not used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeError {
    /// The address, or the code the check reads, is outside the image.
    OutOfImage,
    /// The code has no identity, or not the one the hash gives: the address is not this native's handler.
    NotThisNative { hash: u64 },
}

impl std::fmt::Display for NativeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OutOfImage => f.write_str("the handler is outside the title's image"),
            Self::NotThisNative { hash } => {
                write!(
                    f,
                    "the code there does not identify as the native with hash {hash:#x}"
                )
            }
        }
    }
}

/// How handlers are checked, as the function table describes it.
///
/// A handler passes when, within its first `native_check_window` bytes, a match of one of the guard patterns leads
/// (through the guard's `offset`, `rel8` or `rel32` modifiers, as for any table entry) to a place whose next
/// `native_identity_window` bytes hold the identity `rotl64(hash, native_identity_rotate) ^ native_identity_xor`,
/// little-endian.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeCheck {
    guards: Vec<(Pattern, Entry)>,
    check_window: usize,
    identity_window: usize,
    rotate: u32,
    xor: u64,
}

impl NativeCheck {
    /// The check the table describes; an error names what it lacks.
    pub fn from_table(table: &FunctionTable) -> Result<Self, String> {
        let guards: Vec<(Pattern, Entry)> = table
            .entries
            .iter()
            .filter(|(name, _)| {
                *name == GUARD || name.strip_prefix(GUARD).is_some_and(|r| r.starts_with('_'))
            })
            .filter_map(|(_, entry)| match &entry.locator {
                Locator::Pattern(pattern) => Some((pattern.clone(), entry.clone())),
                Locator::Offset(_) => None,
            })
            .collect();
        if guards.is_empty() {
            return Err(format!("no `{GUARD}` pattern"));
        }
        let missing: Vec<&str> = [CHECK_WINDOW, IDENTITY_WINDOW, IDENTITY_ROTATE, IDENTITY_XOR]
            .into_iter()
            .filter(|name| !table.values.contains_key(*name))
            .collect();
        if !missing.is_empty() {
            return Err(format!("no value for {}", missing.join(", ")));
        }
        let size =
            |name: &str| usize::try_from(table.values[name]).map_err(|_| format!("{name} is too large"));
        Ok(Self {
            guards,
            check_window: size(CHECK_WINDOW)?,
            identity_window: size(IDENTITY_WINDOW)?,
            rotate: u32::try_from(table.values[IDENTITY_ROTATE] % 64).unwrap_or(0),
            xor: table.values[IDENTITY_XOR],
        })
    }

    fn identity(&self, hash: u64) -> u64 {
        hash.rotate_left(self.rotate) ^ self.xor
    }
}

/// A handler checked against its native's hash.
#[derive(Debug, Clone, Copy)]
pub struct Native {
    handler: Handler,
}

impl Native {
    /// The handler at `address`, if its code identifies as the native `hash` by the table's `check`.
    pub fn verified(
        image: &CodeImage<'_>,
        address: u64,
        hash: u64,
        check: &NativeCheck,
    ) -> Result<Self, NativeError> {
        let identity = check.identity(hash).to_le_bytes();
        let start = offset_in(image, address, check.check_window).ok_or(NativeError::OutOfImage)?;
        let head = &image.bytes[start..start + check.check_window];
        let found = check.guards.iter().any(|(pattern, guard)| {
            pattern.find_all(head).any(|at| {
                let target = usize::try_from(at as i64 + guard.offset)
                    .ok()
                    .and_then(|at| follow(guard, head, at))
                    .and_then(|target| start.checked_add(target));
                target
                    .and_then(|t| image.bytes.get(t..t.checked_add(check.identity_window)?))
                    .is_some_and(|tail| tail.windows(identity.len()).any(|w| w == identity))
            })
        });
        if found {
            // SAFETY: the code at `address` is a native handler, as its identity shows, and every handler has the
            // signature `Handler`. Calling it is still unsafe, and only `ScriptScope::call` does.
            let handler = unsafe { std::mem::transmute::<usize, Handler>(address as usize) };
            Ok(Self { handler })
        } else {
            Err(NativeError::NotThisNative { hash })
        }
    }
}

/// The start of `len` bytes at `address` in the image, if they are all inside it.
fn offset_in(image: &CodeImage<'_>, address: u64, len: usize) -> Option<usize> {
    let start = usize::try_from(address.checked_sub(image.base)?).ok()?;
    (start.checked_add(len)? <= image.bytes.len()).then_some(start)
}

/// Where the title keeps a script thread's state, from the function table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScriptLayout {
    /// Where in the title's TLS block the current execution context is kept.
    pub tls_context: usize,
    /// Where a context points to its thread.
    pub context_thread: usize,
    /// Where a thread keeps its state byte.
    pub thread_state: usize,
    /// The states in which the title is updating a thread, when its natives may run: bit N set for state N.
    pub updating: u64,
}

thread_local! {
    /// Whether a scope is open on this thread: scopes do not nest, so each restores what the title had.
    static IN_SCOPE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The calling thread's block of the title's static TLS, from the host.
pub type TitleTls = unsafe extern "C" fn() -> *mut c_void;

/// A script context made current on this thread, for calls; dropping it puts the previous one back.
#[derive(Debug)]
pub struct ScriptScope {
    slot: *mut u64,
    before: u64,
}

impl ScriptScope {
    /// Makes `context` current, if its thread is one the title is updating and no other scope is open on this
    /// thread.
    ///
    /// # Safety
    ///
    /// Called on the thread that just ran `context`, right after the title's script run function returned, with
    /// `title_tls` the host's and `layout` right for the running build. The layout comes from the player's table
    /// and is trusted as given: it is read through without a check, so a wrong value is undefined behaviour, not a
    /// refusal. The scope is dropped on the same thread (it is not `Send`) before the title runs another script.
    pub unsafe fn enter(context: u64, layout: &ScriptLayout, title_tls: TitleTls) -> Option<Self> {
        if context == 0 || IN_SCOPE.get() {
            return None;
        }
        // SAFETY: the caller's contract: a context the title just ran, whose thread pointer is at this offset.
        let thread = unsafe { ((context as usize + layout.context_thread) as *const u64).read() };
        if thread == 0 {
            return None;
        }
        // SAFETY: as above, the thread's state byte.
        let state = unsafe { ((thread as usize + layout.thread_state) as *const u8).read() };
        if state >= 64 || layout.updating & (1 << state) == 0 {
            return None;
        }
        // SAFETY: the host's function, for the calling thread.
        let block = unsafe { title_tls() };
        if block.is_null() {
            return None;
        }
        let slot = (block as usize + layout.tls_context) as *mut u64;
        // SAFETY: the title's TLS block of this thread, at the context's slot; put back on drop.
        let before = unsafe { slot.read() };
        // SAFETY: as above.
        unsafe { slot.write(context) };
        IN_SCOPE.set(true);
        Some(Self { slot, before })
    }
}

/// What natives are called through: a [`ScriptScope`] in the game, a stand-in in tests.
pub trait NativeCaller {
    /// Calls `native` with `args`; its return values land in `ret`.
    ///
    /// # Safety
    ///
    /// `args` are what the native takes (an entity handle is two slots), and `ret` has room for what it returns.
    unsafe fn call(&self, native: Native, args: &mut [u64], ret: &mut [u64]);
}

impl NativeCaller for ScriptScope {
    unsafe fn call(&self, native: Native, args: &mut [u64], ret: &mut [u64]) {
        // SAFETY: the caller's contract, on a script thread with its context current.
        unsafe { native.invoke(args, ret) };
    }
}

impl Native {
    /// Runs the handler.
    ///
    /// # Safety
    ///
    /// As [`NativeCaller::call`], with the title ready to run it on this thread.
    unsafe fn invoke(self, args: &mut [u64], ret: &mut [u64]) {
        ret.fill(0);
        let mut call = Call {
            args: args.as_mut_ptr(),
            ret: ret.as_mut_ptr(),
            argc: i32::try_from(args.len()).unwrap_or(i32::MAX),
            retc: u32::try_from(ret.len()).unwrap_or(u32::MAX),
        };
        // SAFETY: a verified handler, given what it takes.
        unsafe { (self.handler)(&mut call) };
    }

    /// A native from a test's own handler, without the check.
    #[cfg(test)]
    pub(crate) fn from_handler(handler: Handler) -> Self {
        Self { handler }
    }
}

/// Calls a test's handlers directly, with no script context.
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct Direct;

#[cfg(test)]
impl NativeCaller for Direct {
    unsafe fn call(&self, native: Native, args: &mut [u64], ret: &mut [u64]) {
        // SAFETY: a test's handler, which needs no script context.
        unsafe { native.invoke(args, ret) };
    }
}

impl Drop for ScriptScope {
    fn drop(&mut self) {
        // SAFETY: the slot `enter` wrote, on the same thread.
        unsafe { self.slot.write(self.before) };
        IN_SCOPE.set(false);
    }
}

/// A `Vector3` return: x and y in the first slot, z in the low half of the second.
pub fn vector3(ret: &[u64]) -> [f32; 3] {
    let (a, b) = (
        ret.first().copied().unwrap_or(0),
        ret.get(1).copied().unwrap_or(0),
    );
    [
        f32::from_bits(a as u32),
        f32::from_bits((a >> 32) as u32),
        f32::from_bits(b as u32),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: u64 = 0xA86D_5F06_9399_F44D;

    /// A made-up check, nothing like any title's: guard `D1 D2 ?? D3` whose third byte is a short jump from the
    /// guard's end, or `E1 E2` and a near jump after it; the identity is the hash rotated by 8, xored with a mask.
    fn check() -> NativeCheck {
        let table = FunctionTable::parse(
            "build = x\n\
             native_guard = D1 D2 ?? D3 ; rel8=2,4\n\
             native_guard_near = E1 E2 ?? ?? ?? ?? ; rel32=2,6\n\
             value native_check_window = 0x80\n\
             value native_identity_window = 0x20\n\
             value native_identity_rotate = 8\n\
             value native_identity_xor = 0x00FF00FF00FF00FF\n",
        )
        .expect("table");
        NativeCheck::from_table(&table).expect("check")
    }

    fn identity(hash: u64) -> [u8; 8] {
        (hash.rotate_left(8) ^ 0x00FF_00FF_00FF_00FF).to_le_bytes()
    }

    /// Made-up handler code: filler, the short guard leading 0x30 bytes on, and the identity there.
    fn handler_code(hash: u64) -> Vec<u8> {
        let mut code = vec![0x90; 0x100];
        code[4..8].copy_from_slice(&[0xD1, 0xD2, 0x30, 0xD3]);
        let at = 8 + 0x30 + 3;
        code[at..at + 8].copy_from_slice(&identity(hash));
        code
    }

    fn image(bytes: &[u8]) -> CodeImage<'_> {
        CodeImage {
            base: 0x1_4000_0000,
            bytes,
        }
    }

    #[test]
    fn a_handler_is_taken_only_for_its_own_hash() {
        let code = handler_code(HASH);
        let image = image(&code);
        let check = check();
        assert!(Native::verified(&image, image.base, HASH, &check).is_ok());
        assert_eq!(
            Native::verified(&image, image.base, HASH ^ 1, &check).err(),
            Some(NativeError::NotThisNative { hash: HASH ^ 1 })
        );
        // Too near the image's end to read, or before it, it is refused.
        assert_eq!(
            Native::verified(&image, image.base + 0x90, HASH, &check).err(),
            Some(NativeError::OutOfImage)
        );
        assert_eq!(
            Native::verified(&image, image.base - 1, HASH, &check).err(),
            Some(NativeError::OutOfImage)
        );
    }

    #[test]
    fn every_guard_is_tried() {
        let mut code = vec![0x90; 0x100];
        code[0x10..0x12].copy_from_slice(&[0xE1, 0xE2]);
        code[0x12..0x16].copy_from_slice(&0x40_i32.to_le_bytes());
        let at = 0x16 + 0x40;
        code[at..at + 8].copy_from_slice(&identity(HASH));
        let image = image(&code);
        assert!(Native::verified(&image, image.base, HASH, &check()).is_ok());
    }

    #[test]
    fn without_a_guard_there_is_no_identity() {
        let mut code = handler_code(HASH);
        code[5] = 0x00;
        let image = image(&code);
        assert!(Native::verified(&image, image.base, HASH, &check()).is_err());
    }

    #[test]
    fn the_check_needs_its_guard_and_values() {
        let table = FunctionTable::parse("build = x\nvalue native_check_window = 0x80\n").expect("table");
        let error = NativeCheck::from_table(&table).expect_err("incomplete");
        assert!(error.contains("native_guard"), "{error}");
        let table =
            FunctionTable::parse("build = x\nnative_guard_a = D1\nvalue native_check_window = 0x80\n")
                .expect("table");
        let error = NativeCheck::from_table(&table).expect_err("incomplete");
        assert!(error.contains("native_identity_window"), "{error}");
        // `native_guards` is another entry, not a guard.
        let table = FunctionTable::parse("build = x\nnative_guards = D1\n").expect("table");
        assert!(NativeCheck::from_table(&table).is_err());
    }

    #[test]
    fn a_vector_is_unpacked() {
        let ret = [
            u64::from(1.5f32.to_bits()) | (u64::from((-2.0f32).to_bits()) << 32),
            u64::from(4.25f32.to_bits()) | 0xFFFF_FFFF_0000_0000,
        ];
        assert_eq!(vector3(&ret), [1.5, -2.0, 4.25]);
    }

    /// A stand-in for the title: its TLS block, a context, its thread, and a handler that reads the context.
    static mut BLOCK: [u64; 4] = [0, 0, 0xAAAA, 0];
    static SEEN: std::sync::Mutex<Vec<(u64, i32, u64)>> = std::sync::Mutex::new(Vec::new());

    unsafe extern "C" fn title_tls() -> *mut c_void {
        std::ptr::addr_of_mut!(BLOCK).cast()
    }

    unsafe extern "C" fn echo(call: *mut Call) {
        // SAFETY: the bridge passes a valid call; the block is the test's.
        unsafe {
            let call = &mut *call;
            let current = (*std::ptr::addr_of!(BLOCK))[2];
            let arg = *call.args;
            SEEN.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((current, call.argc, arg));
            *call.ret = arg + 1;
        }
    }

    #[test]
    fn a_call_runs_with_the_context_current_and_puts_it_back() {
        let layout = ScriptLayout {
            tls_context: 16,
            context_thread: 8,
            thread_state: 4,
            updating: 0b1100,
        };
        let thread: *mut [u8; 8] = Box::into_raw(Box::new([0u8; 8]));
        let mut context = [0u64; 2];
        context[1] = thread as u64;
        let ctx = context.as_ptr() as u64;
        let native = Native { handler: echo };

        // A thread the title is not updating: no scope.
        // SAFETY: the stand-ins above match the layout.
        assert!(unsafe { ScriptScope::enter(ctx, &layout, title_tls) }.is_none());
        assert!(unsafe { ScriptScope::enter(0, &layout, title_tls) }.is_none());

        // SAFETY: the test's thread, written through the pointer the context holds.
        unsafe { (*thread)[4] = 2 };
        // SAFETY: as above.
        let scope = unsafe { ScriptScope::enter(ctx, &layout, title_tls) }.expect("updating");
        // A second scope on the same thread is refused, so the first restores what the title had.
        // SAFETY: as above.
        assert!(unsafe { ScriptScope::enter(ctx, &layout, title_tls) }.is_none());
        let mut ret = [9u64; 2];
        // SAFETY: `echo` takes one slot and returns one.
        unsafe { scope.call(native, &mut [41], &mut ret) };
        assert_eq!(ret, [42, 0]);
        drop(scope);
        assert_eq!(
            *SEEN.lock().unwrap_or_else(std::sync::PoisonError::into_inner),
            vec![(ctx, 1, 41)]
        );
        // SAFETY: the test's block, read after the scope is gone.
        assert_eq!(unsafe { (*std::ptr::addr_of!(BLOCK))[2] }, 0xAAAA);
        // SAFETY: made by `Box::into_raw` above and no longer used.
        drop(unsafe { Box::from_raw(thread) });
    }
}
