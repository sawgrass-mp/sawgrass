//! Calls into the title's script natives, the way the title's own scripts call them.
//!
//! A native is a handler function the title runs for one script command: `handler(&mut Call)`, with the arguments
//! and the return values in arrays of 64-bit slots. A handler runs only on a script thread the title is updating,
//! with that thread's execution context current in the title's thread-local storage, so a call goes through a
//! [`ScriptScope`]: entered right after the title ran a script context on this thread, it makes that context current,
//! and puts back what was there when it is dropped.
//!
//! Nothing about the title is built in. Every address, offset and hash comes from the function table on the
//! player's machine, and before a handler is used it is checked against its hash ([`Native::verified`]): a handler
//! answers a call with a negative argument count by writing its identity, `!rotl64(hash, 32)`, and that constant is
//! read from its code, without calling it. A wrong address in a table is refused instead of called.

use std::ffi::c_void;

use crate::runtime::CodeImage;

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

/// How far into a handler the argument-count check is looked for, and how far past the branch its identity.
const HEAD: usize = 0x100;
const TAIL: usize = 0x40;
/// `cmp dword [rcx+10h], 0`: the handler compares `Call::argc` (at 0x10) with zero.
const ARGC_CHECK: [u8; 4] = [0x83, 0x79, 0x10, 0x00];
/// How many bytes after that compare the `js` that takes the negative count is looked for.
const BRANCH_WINDOW: usize = 16;

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

/// A handler checked against its native's hash.
#[derive(Debug, Clone, Copy)]
pub struct Native {
    handler: Handler,
}

impl Native {
    /// The handler at `address`, if its code identifies as the native `hash`.
    ///
    /// The check is on bytes, not a full decode: after `cmp dword [rcx+10h], 0` comes a `js` (short or near), and
    /// within the first instructions at its target a `mov r64, imm64` loads the identity. A false match would need
    /// the exact 64-bit identity at such a place, so a wrong address is refused.
    pub fn verified(image: &CodeImage<'_>, address: u64, hash: u64) -> Result<Self, NativeError> {
        let identity = !hash.rotate_left(32);
        let start = offset_in(image, address, HEAD).ok_or(NativeError::OutOfImage)?;
        let head = &image.bytes[start..start + HEAD];
        let found = head
            .windows(ARGC_CHECK.len())
            .enumerate()
            .filter(|(_, w)| *w == ARGC_CHECK)
            .flat_map(|(at, _)| branch_targets(head, at + ARGC_CHECK.len()))
            .filter_map(|target| offset_in(image, address.checked_add_signed(target as i64)?, TAIL))
            .any(|tail| loads_immediate(&image.bytes[tail..tail + TAIL], identity));
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

/// Targets, relative to the start of `code`, of each `js` that may begin in the window after `from`.
fn branch_targets(code: &[u8], from: usize) -> impl Iterator<Item = isize> + '_ {
    (from..(from + BRANCH_WINDOW).min(code.len())).filter_map(move |at| {
        let rest = &code[at..];
        match rest {
            [0x78, rel, ..] => Some(at as isize + 2 + isize::from(*rel as i8)),
            [0x0F, 0x88, a, b, c, d, ..] => {
                Some(at as isize + 6 + i32::from_le_bytes([*a, *b, *c, *d]) as isize)
            }
            _ => None,
        }
    })
}

/// Whether `code` holds a `mov r64, imm64` (REX.W, `B8+r`) of `value`.
fn loads_immediate(code: &[u8], value: u64) -> bool {
    code.windows(10).any(|w| {
        matches!(w[0], 0x48 | 0x49) && (0xB8..=0xBF).contains(&w[1]) && w[2..10] == value.to_le_bytes()
    })
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
}

/// The thread states in which the title is updating a script thread, when its natives may run.
const UPDATING: [u8; 2] = [2, 3];

/// The calling thread's block of the title's static TLS, from the host.
pub type TitleTls = unsafe extern "C" fn() -> *mut c_void;

/// A script context made current on this thread, for calls; dropping it puts the previous one back.
#[derive(Debug)]
pub struct ScriptScope {
    slot: *mut u64,
    before: u64,
}

impl ScriptScope {
    /// Makes `context` current, if its thread is one the title is updating.
    ///
    /// # Safety
    ///
    /// Called on the thread that just ran `context`, right after the title's script run function returned, with
    /// `layout` and `title_tls` right for the running build. The scope is dropped on the same thread before the
    /// title runs another script.
    pub unsafe fn enter(context: u64, layout: &ScriptLayout, title_tls: TitleTls) -> Option<Self> {
        if context == 0 {
            return None;
        }
        // SAFETY: the caller's contract: a context the title just ran, whose thread pointer is at this offset.
        let thread = unsafe { ((context as usize + layout.context_thread) as *const u64).read() };
        if thread == 0 {
            return None;
        }
        // SAFETY: as above, the thread's state byte.
        let state = unsafe { ((thread as usize + layout.thread_state) as *const u8).read() };
        if !UPDATING.contains(&state) {
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
        Some(Self { slot, before })
    }

    /// Calls `native` with `args`; its return values land in `ret`.
    ///
    /// # Safety
    ///
    /// `args` are what the native takes (an entity handle is two slots), and `ret` has room for what it returns.
    pub unsafe fn call(&self, native: Native, args: &mut [u64], ret: &mut [u64]) {
        ret.fill(0);
        let mut call = Call {
            args: args.as_mut_ptr(),
            ret: ret.as_mut_ptr(),
            argc: i32::try_from(args.len()).unwrap_or(i32::MAX),
            retc: u32::try_from(ret.len()).unwrap_or(u32::MAX),
        };
        // SAFETY: a verified handler, on a script thread with its context current, given what it takes.
        unsafe { (native.handler)(&mut call) };
    }
}

impl Drop for ScriptScope {
    fn drop(&mut self) {
        // SAFETY: the slot `enter` wrote, on the same thread.
        unsafe { self.slot.write(self.before) };
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

    /// Made-up handler code: a prologue, the count check, a short `js` and, at its target, the identity load.
    fn handler_code(hash: u64) -> Vec<u8> {
        let mut code = vec![0xCC; 0x200];
        // sub rsp, 28h ; mov rbx, rcx ; cmp dword [rcx+10h], 0 ; js +40h ; ret
        let head = [
            0x48, 0x83, 0xEC, 0x28, 0x48, 0x89, 0xCB, 0x83, 0x79, 0x10, 0x00, 0x78, 0x40, 0xC3,
        ];
        code[..head.len()].copy_from_slice(&head);
        // mov rax, [rcx] ; mov rcx, imm64 ; mov [rax], rcx
        let at = head.len() + 0x40;
        let mut tail = vec![0x48, 0x8B, 0x01, 0x48, 0xB9];
        tail.extend((!hash.rotate_left(32)).to_le_bytes());
        tail.extend([0x48, 0x89, 0x08]);
        code[at..at + tail.len()].copy_from_slice(&tail);
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
        assert!(Native::verified(&image, image.base, HASH).is_ok());
        assert_eq!(
            Native::verified(&image, image.base, HASH ^ 1).err(),
            Some(NativeError::NotThisNative { hash: HASH ^ 1 })
        );
        // Too near the image's end to read, or before it, it is refused.
        assert_eq!(
            Native::verified(&image, image.base + 0x180, HASH).err(),
            Some(NativeError::OutOfImage)
        );
        assert_eq!(
            Native::verified(&image, image.base - 1, HASH).err(),
            Some(NativeError::OutOfImage)
        );
    }

    #[test]
    fn a_near_branch_is_followed_too() {
        let mut code = handler_code(HASH);
        // cmp ; js rel32 to the same target (0x4E) ; ret
        let head = [0x83, 0x79, 0x10, 0x00, 0x0F, 0x88];
        code[..6].copy_from_slice(&head);
        code[6..10].copy_from_slice(&(0x4E_i32 - 10).to_le_bytes());
        code[10] = 0xC3;
        code[11..14].fill(0xCC);
        let image = image(&code);
        assert!(Native::verified(&image, image.base, HASH).is_ok());
    }

    #[test]
    fn without_the_count_check_there_is_no_identity() {
        let mut code = handler_code(HASH);
        code[9] = 0x14;
        let image = image(&code);
        assert!(Native::verified(&image, image.base, HASH).is_err());
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
