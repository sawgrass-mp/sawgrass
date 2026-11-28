//! Peds spawned through the title's own spawn request function, which its create-ped natives queue into.
//!
//! The function takes a request (where, which model, which script thread owns the ped) and three callbacks, and the
//! title creates the ped about a second later. The request has fields only the title fills well (the model's
//! index and variation), so the adapter does not build one from nothing: it hooks the function, keeps the last
//! request the title's own create path made as a template, and asks again with that template at another place.
//! Everything that names the title (the function, the callback the title passes for a spawned ped, the lock the
//! title takes around the call, the request's layout) comes from the table on the player's machine; none of it is
//! in this repository.
//!
//! The answer is not a ped but a queued request, so [`Spawner::spawn`] answers [`GameError::Refused`] until the ped
//! is there: the network core asks again on the next frames, and once a ped stands at the asked place (found with
//! the closest-ped native) that ped is the spawned one.
//!
//! ASSUMPTION: the ped spawned has the template's model, whatever asset the server named; picking the model needs
//! the title's model index, which the template does not give for another model. And a request kept from some
//! script threads gives no ped; another request is asked for after [`WAIT`].

use std::cell::RefCell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::game::GameError;
use crate::table::FunctionTable;

/// Table names: the request function (the module's `ped_spawn`, hooked), the callback the title passes it for a
/// spawned ped and that callback's copy and drop functions, the lock and its two functions, and the natives that
/// find the spawned ped and turn it.
pub const DONE: &str = "ped_spawn_done";
pub const DONE_KEEP: &str = "ped_spawn_done_keep";
pub const DONE_DROP: &str = "ped_spawn_done_drop";
pub const LOCK: &str = "native_lock";
pub const LOCK_ENTER: &str = "native_lock_enter";
pub const LOCK_LEAVE: &str = "native_lock_leave";
pub const CLOSEST_PED: &str = "ped_closest";
pub const SET_HEADING: &str = "entity_set_heading";
/// Value names: the request's size, the kind word that marks the title's create path, and the offsets of the
/// position, the owner thread and the model; and the request's bytes the title's caller leaves stale, cleared.
pub const SIZE: &str = "ped_request_size";
pub const KIND: &str = "ped_request_kind";
pub const POSITION: &str = "ped_request_position";
pub const OWNER: &str = "ped_request_owner";
pub const MODEL: &str = "ped_request_model";
pub const STALE_FROM: &str = "ped_request_stale_from";
pub const STALE_TO: &str = "ped_request_stale_to";
/// Optional: one more stale 8-byte word, outside that range.
pub const STALE_WORD: &str = "ped_request_stale_word";

/// Every function name the spawner needs, beside `ped_spawn`.
pub const FUNCTIONS: [&str; 6] = [DONE, DONE_KEEP, DONE_DROP, LOCK, LOCK_ENTER, LOCK_LEAVE];
const VALUES: [&str; 7] = [SIZE, KIND, POSITION, OWNER, MODEL, STALE_FROM, STALE_TO];

/// How long a request is waited for before another one is made.
pub const WAIT: Duration = Duration::from_secs(3);
/// How far from the asked place the spawned ped may stand, in metres.
pub const NEAR: f32 = 1.5;
/// Requests in a row that gave no ped found before the spawner pauses: each one leaves a ped in the world, so a
/// ped never found would otherwise grow a crowd.
pub const MAX_UNFOUND: u32 = 3;
/// How long the spawner pauses after [`MAX_UNFOUND`] requests gave no ped found.
pub const PAUSE: Duration = Duration::from_secs(30);

/// The request's layout, from the table's values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    pub size: usize,
    pub kind: u64,
    pub position: usize,
    pub owner: usize,
    pub model: usize,
    pub stale: (usize, usize),
    pub stale_word: Option<usize>,
}

impl Layout {
    /// The layout, if the table has every value and they fit in the request.
    pub fn from_table(table: &FunctionTable) -> Result<Self, String> {
        let missing: Vec<&str> = VALUES
            .into_iter()
            .filter(|n| !table.values.contains_key(*n))
            .collect();
        if !missing.is_empty() {
            return Err(format!("no value for {}", missing.join(", ")));
        }
        let at = |name: &str| usize::try_from(table.values[name]).unwrap_or(usize::MAX);
        let layout = Self {
            size: at(SIZE),
            kind: table.values[KIND],
            position: at(POSITION),
            owner: at(OWNER),
            model: at(MODEL),
            stale: (at(STALE_FROM), at(STALE_TO)),
            stale_word: table
                .values
                .get(STALE_WORD)
                .map(|v| usize::try_from(*v).unwrap_or(usize::MAX)),
        };
        let fits = |at: usize, len: usize| at.checked_add(len).is_some_and(|end| end <= layout.size);
        let ok = layout.size <= MAX_SIZE
            && fits(layout.position, 12)
            && fits(layout.owner, 4)
            && fits(layout.model, 4)
            && layout.stale.0 <= layout.stale.1
            && layout.stale.1 <= layout.size
            && layout.stale_word.is_none_or(|at| fits(at, 8));
        if ok {
            Ok(layout)
        } else {
            Err("a request value is outside the request".into())
        }
    }
}

/// The largest request kept.
const MAX_SIZE: usize = 0x400;

/// The addresses the spawner calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Functions {
    /// The request function's original code (the hook's trampoline), read when called.
    pub done: u64,
    pub done_keep: u64,
    pub done_drop: u64,
    pub lock: u64,
    pub lock_enter: u64,
    pub lock_leave: u64,
}

type Request = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64) -> u64;
type LockFn = unsafe extern "C" fn(u64);

/// The request function's original code, written by the host before its hook goes live.
pub(super) static ORIGINAL: AtomicU64 = AtomicU64::new(0);
/// The layout the hook reads requests with, set before the hook goes live.
static LAYOUT: Mutex<Option<Layout>> = Mutex::new(None);
/// The last request of the title's own create path.
static TEMPLATE: Mutex<Option<Vec<u8>>> = Mutex::new(None);

/// Sets the layout the hook keeps templates with. Called before the hook is installed.
pub(super) fn set_layout(layout: Layout) {
    *LAYOUT.lock().unwrap_or_else(PoisonError::into_inner) = Some(layout);
}

/// The detour on the request function: keeps the title's own create-path requests, then lets the call through.
pub(super) unsafe extern "C" fn request_detour(
    a: u64,
    b: u64,
    c: u64,
    request: u64,
    kind: u64,
    f: u64,
) -> u64 {
    // SAFETY: `request` is the title's request block, valid for the call; `keep` reads only its layout's size.
    let _ = std::panic::catch_unwind(|| unsafe { keep(request, kind) });
    // SAFETY: the host writes the trampoline before the hook goes live; `Option` of a function pointer has the
    // null niche.
    let original =
        unsafe { std::mem::transmute::<usize, Option<Request>>(ORIGINAL.load(Ordering::Acquire) as usize) };
    match original {
        // SAFETY: the original function, with the title's arguments.
        Some(original) => unsafe { original(a, b, c, request, kind, f) },
        None => u64::MAX,
    }
}

/// Keeps `request` as the template if it is one of the title's create-path requests.
///
/// # Safety
///
/// `request` points to at least the layout's size of readable bytes, or is null.
unsafe fn keep(request: u64, kind: u64) {
    let Some(layout) = *LAYOUT.lock().unwrap_or_else(PoisonError::into_inner) else {
        return;
    };
    if request == 0 || kind & 0xffff != layout.kind {
        return;
    }
    // SAFETY: the caller's contract.
    let bytes = unsafe { std::slice::from_raw_parts(request as *const u8, layout.size) }.to_vec();
    let word = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap_or_default());
    // The create path's own requests carry the kind in their first word too, and a model.
    if u64::from(word(0) & 0xffff) != layout.kind || word(layout.model) == 0 {
        return;
    }
    *TEMPLATE.lock().unwrap_or_else(PoisonError::into_inner) = Some(bytes);
}

/// A request made and not yet seen as a ped.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Pending {
    at: [f32; 3],
    since: Instant,
}

/// What spawns peds, for the whole run.
#[derive(Debug)]
pub struct Spawner {
    functions: Functions,
    layout: Layout,
    pending: RefCell<Option<Pending>>,
    /// The first slots of the peds spawned so far, so that a ped found at a place is never handed out twice.
    spawned: RefCell<Vec<u64>>,
    /// What the requests did so far, for the log.
    pub report: RefCell<Report>,
    /// Requests in a row that gave no ped found, and when the last of them was made.
    unfound: RefCell<(u32, Option<Instant>)>,
}

/// What the spawner did, for the log.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Report {
    /// Requests the title took.
    pub requests: u64,
    /// The title's answer to the last request.
    pub last_id: i32,
    /// The owner thread of the template the last request was made from.
    pub owner: u32,
    /// How far from the awaited place the closest ped stood when it was last looked for, in metres, if any stood
    /// within the search radius.
    pub closest: Option<f32>,
}

impl std::fmt::Display for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} spawn requests, last answer {}, template owner {:#x}",
            self.requests, self.last_id, self.owner
        )?;
        match self.closest {
            Some(d) => write!(f, ", closest ped {d:.1} m from the asked place"),
            None => f.write_str(", no ped near the asked place"),
        }
    }
}

impl Spawner {
    pub fn new(functions: Functions, layout: Layout) -> Self {
        Self {
            functions,
            layout,
            pending: RefCell::new(None),
            spawned: RefCell::new(Vec::new()),
            report: RefCell::new(Report::default()),
            unfound: RefCell::new((0, None)),
        }
    }

    /// The place a request awaits a ped at, if one does.
    pub fn awaited(&self) -> Option<[f32; 3]> {
        self.pending.borrow().map(|p| p.at)
    }

    /// Whether the ped `raw` names was spawned here before.
    pub fn is_spawned(&self, raw: u64) -> bool {
        self.spawned.borrow().contains(&raw)
    }

    /// Marks the awaited request done: its ped `raw` was found.
    pub fn found(&self, raw: u64) {
        self.pending.borrow_mut().take();
        self.spawned.borrow_mut().push(raw);
        *self.unfound.borrow_mut() = (0, None);
    }

    /// Makes a request at `at` unless one is awaited, or one awaited longer than [`WAIT`] is given up for a new one.
    ///
    /// # Safety
    ///
    /// On one of the title's script threads, with the table's addresses for this build.
    pub unsafe fn request(&self, at: [f32; 3], now: Instant) -> Result<(), GameError> {
        if let Some(p) = *self.pending.borrow() {
            if now.duration_since(p.since) < WAIT {
                return Ok(());
            }
        }
        let (unfound, last) = *self.unfound.borrow();
        if unfound >= MAX_UNFOUND {
            match last {
                Some(last) if now.duration_since(last) < WAIT + PAUSE => {
                    return Err(GameError::Refused(format!(
                        "paused: {unfound} spawn requests in a row gave no ped found"
                    )));
                }
                _ => *self.unfound.borrow_mut() = (0, None),
            }
        }
        let Some(mut request) = TEMPLATE.lock().unwrap_or_else(PoisonError::into_inner).clone() else {
            return Err(GameError::Refused(
                "no template yet: the game has not spawned a ped of its own since the start".into(),
            ));
        };
        let l = self.layout;
        request[l.stale.0..l.stale.1].fill(0);
        if let Some(at) = l.stale_word {
            request[at..at + 8].fill(0);
        }
        for (i, v) in at.iter().enumerate() {
            // x and y in one slot, z in the low half of the next, as the title's natives pack a vector.
            let p = l.position + 4 * i;
            if let Some(slot) = request.get_mut(p..p + 4) {
                slot.copy_from_slice(&v.to_le_bytes());
            }
        }
        let owner = u32::from_le_bytes(request[l.owner..l.owner + 4].try_into().unwrap_or_default());
        // The on-spawn callback: its data is the owner thread, and the ped is not hidden (the other bytes zero).
        let mut data = u64::from(owner);
        let f = self.functions;
        let mut done = [&raw mut data as u64, f.done, f.done_keep, f.done_drop];
        let (mut none_b, mut none_c, mut none_d) = ([0u64; 4], [0u64; 4], [0u64; 4]);
        // SAFETY: the table's addresses for this build: the lock functions take the lock, and the request function
        // the title's original code; the callbacks and the request live for the call.
        let id = unsafe {
            let enter = std::mem::transmute::<usize, LockFn>(f.lock_enter as usize);
            let leave = std::mem::transmute::<usize, LockFn>(f.lock_leave as usize);
            let call =
                std::mem::transmute::<usize, Option<Request>>(ORIGINAL.load(Ordering::Acquire) as usize);
            let Some(call) = call else {
                return Err(GameError::Refused("ped_spawn is not hooked".into()));
            };
            enter(f.lock);
            let id = call(
                done.as_mut_ptr() as u64,
                none_b.as_mut_ptr() as u64,
                none_c.as_mut_ptr() as u64,
                request.as_mut_ptr() as u64,
                l.kind,
                none_d.as_mut_ptr() as u64,
            );
            leave(f.lock);
            id as u32 as i32
        };
        {
            let mut report = self.report.borrow_mut();
            report.last_id = id;
            report.owner = owner;
            if id >= 0 {
                report.requests += 1;
            }
        }
        if id < 0 {
            return Err(GameError::Refused(format!(
                "the game refused the spawn request ({id})"
            )));
        }
        *self.pending.borrow_mut() = Some(Pending { at, since: now });
        let mut unfound = self.unfound.borrow_mut();
        *unfound = (unfound.0 + 1, Some(now));
        Ok(())
    }
}

/// Whether two places are within [`NEAR`] of each other on the ground.
pub fn near(a: [f32; 3], b: [f32; 3]) -> bool {
    (a[0] - b[0]).hypot(a[1] - b[1]) < NEAR
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// One request the stand-in title got: its bytes, its kind, and the on-spawn callback's data and functions.
    pub(crate) type Seen = (Vec<u8>, u64, [u64; 4]);
    pub(crate) static REQUESTS: Mutex<Vec<Seen>> = Mutex::new(Vec::new());
    pub(crate) static LOCKS: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());

    pub(crate) const LAYOUT_FOR_TESTS: Layout = Layout {
        size: 0x40,
        kind: 1,
        position: 0x10,
        owner: 0x30,
        model: 0x20,
        stale: (0x38, 0x3c),
        stale_word: Some(0x08),
    };

    unsafe extern "C" fn title_request(a: u64, _: u64, _: u64, request: u64, kind: u64, _: u64) -> u64 {
        // SAFETY: the spawner's call: a request of the test layout's size and four callback slots.
        let (bytes, done) = unsafe {
            (
                std::slice::from_raw_parts(request as *const u8, LAYOUT_FOR_TESTS.size).to_vec(),
                *(a as *const [u64; 4]),
            )
        };
        let data = if done[0] == 0 {
            0
        } else {
            unsafe { *(done[0] as *const u64) }
        };
        REQUESTS.lock().unwrap_or_else(PoisonError::into_inner).push((
            bytes,
            kind,
            [data, done[1], done[2], done[3]],
        ));
        7
    }

    unsafe extern "C" fn enter(_: u64) {
        LOCKS.lock().unwrap_or_else(PoisonError::into_inner).push("enter");
    }

    unsafe extern "C" fn leave(_: u64) {
        LOCKS.lock().unwrap_or_else(PoisonError::into_inner).push("leave");
    }

    pub(crate) fn functions() -> Functions {
        Functions {
            done: 0xd0,
            done_keep: 0xd1,
            done_drop: 0xd2,
            lock: 0x10c,
            lock_enter: enter as *const () as u64,
            lock_leave: leave as *const () as u64,
        }
    }

    /// The title's own request: kind 1, a model, an owner, stale bytes, at (1, 2, 3).
    pub(crate) fn title_template() -> Vec<u8> {
        let mut r = vec![0u8; LAYOUT_FOR_TESTS.size];
        r[0] = 1;
        r[0x10..0x14].copy_from_slice(&1.0f32.to_le_bytes());
        r[0x14..0x18].copy_from_slice(&2.0f32.to_le_bytes());
        r[0x18..0x1c].copy_from_slice(&3.0f32.to_le_bytes());
        r[0x20..0x24].copy_from_slice(&0x1234_5678u32.to_le_bytes());
        r[0x30..0x34].copy_from_slice(&0x1cu32.to_le_bytes());
        r[0x08..0x10].fill(0xee);
        r[0x38..0x40].fill(0xee);
        r
    }

    /// Hooks the stand-in title and has it make one request of its own.
    pub(crate) fn title_spawns_once() {
        set_layout(LAYOUT_FOR_TESTS);
        ORIGINAL.store(title_request as *const () as u64, Ordering::Release);
        let mut own = title_template();
        let mut done = [0u64; 4];
        // SAFETY: the detour with the stand-in title's arguments.
        unsafe { request_detour(done.as_mut_ptr() as u64, 0, 0, own.as_mut_ptr() as u64, 1, 0) };
    }

    fn values(table: &mut FunctionTable, pairs: &[(&str, u64)]) {
        for (k, v) in pairs {
            table.values.insert((*k).to_owned(), *v);
        }
    }

    #[test]
    fn the_layout_comes_from_the_table_and_must_fit() {
        let mut table = FunctionTable::parse("build = victory/1/2").expect("table");
        assert!(Layout::from_table(&table).is_err());
        values(
            &mut table,
            &[
                (SIZE, 0x40),
                (KIND, 1),
                (POSITION, 0x10),
                (OWNER, 0x30),
                (MODEL, 0x20),
                (STALE_FROM, 0x38),
                (STALE_TO, 0x3c),
                (STALE_WORD, 0x08),
            ],
        );
        assert_eq!(Layout::from_table(&table), Ok(LAYOUT_FOR_TESTS));
        values(&mut table, &[(OWNER, 0x3e)]);
        assert!(
            Layout::from_table(&table).is_err(),
            "the owner would run past the request"
        );
    }

    #[test]
    fn asks_again_with_the_title_template_at_the_new_place() {
        let _serial = crate::victory::game::tests::SERIAL
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        REQUESTS.lock().unwrap_or_else(PoisonError::into_inner).clear();
        title_spawns_once();
        let spawner = Spawner::new(functions(), LAYOUT_FOR_TESTS);
        let now = Instant::now();
        // SAFETY: the stand-in title's functions.
        let step = unsafe { spawner.request([10.0, 20.0, 30.0], now) };
        assert_eq!(step, Ok(()));
        assert_eq!(spawner.awaited(), Some([10.0, 20.0, 30.0]));
        let requests = REQUESTS.lock().unwrap_or_else(PoisonError::into_inner).clone();
        assert_eq!(requests.len(), 2, "the title's own, then ours");
        let (ours, kind, done) = &requests[1];
        assert_eq!(*kind, 1);
        let f = |at: usize| f32::from_le_bytes(ours[at..at + 4].try_into().expect("four bytes"));
        assert_eq!((f(0x10), f(0x14), f(0x18)), (10.0, 20.0, 30.0));
        assert_eq!(
            ours[0x20..0x24],
            0x1234_5678u32.to_le_bytes(),
            "the template's model"
        );
        assert!(ours[0x38..0x3c].iter().all(|&b| b == 0), "stale bytes cleared");
        assert!(ours[0x08..0x10].iter().all(|&b| b == 0), "the stale word too");
        assert_eq!(ours[0x3c..0x40], [0xee; 4], "and nothing else");
        assert_eq!(
            *done,
            [0x1c, 0xd0, 0xd1, 0xd2],
            "the on-spawn callback, owned by the template's thread"
        );
        // Waited for, not asked again, until the wait is over.
        // SAFETY: as above.
        unsafe {
            spawner
                .request([10.0, 20.0, 30.0], now + Duration::from_secs(1))
                .expect("waits")
        };
        assert_eq!(REQUESTS.lock().unwrap_or_else(PoisonError::into_inner).len(), 2);
        // SAFETY: as above.
        unsafe {
            spawner
                .request([10.0, 20.0, 30.0], now + WAIT)
                .expect("asks again")
        };
        assert_eq!(REQUESTS.lock().unwrap_or_else(PoisonError::into_inner).len(), 3);
        // A third request with no ped found, then the spawner pauses instead of growing a crowd.
        // SAFETY: as above.
        unsafe {
            spawner
                .request([10.0, 20.0, 30.0], now + WAIT * 2)
                .expect("asks a third time")
        };
        // SAFETY: as above.
        let paused = unsafe { spawner.request([10.0, 20.0, 30.0], now + WAIT * 3) };
        assert!(matches!(paused, Err(GameError::Refused(_))), "{paused:?}");
        assert_eq!(REQUESTS.lock().unwrap_or_else(PoisonError::into_inner).len(), 4);
        assert_eq!(spawner.report.borrow().requests, 3);
        // SAFETY: as above.
        unsafe {
            spawner
                .request([10.0, 20.0, 30.0], now + WAIT * 3 + PAUSE)
                .expect("asks again after the pause")
        };
        assert_eq!(REQUESTS.lock().unwrap_or_else(PoisonError::into_inner).len(), 5);
        let locks = LOCKS.lock().unwrap_or_else(PoisonError::into_inner).clone();
        assert!(
            locks.ends_with(&["enter", "leave", "enter", "leave"]),
            "{locks:?}"
        );
    }

    #[test]
    fn other_requests_are_not_kept() {
        let _serial = crate::victory::game::tests::SERIAL
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        set_layout(LAYOUT_FOR_TESTS);
        *TEMPLATE.lock().unwrap_or_else(PoisonError::into_inner) = None;
        let mut loading = title_template();
        loading[0] = 5;
        // SAFETY: a request of the layout's size.
        unsafe { keep(loading.as_mut_ptr() as u64, 0x30) };
        unsafe { keep(loading.as_mut_ptr() as u64, 1) };
        assert!(TEMPLATE.lock().unwrap_or_else(PoisonError::into_inner).is_none());
        let spawner = Spawner::new(functions(), LAYOUT_FOR_TESTS);
        // SAFETY: nothing is called without a template.
        assert!(matches!(
            unsafe { spawner.request([0.0; 3], Instant::now()) },
            Err(GameError::Refused(_))
        ));
    }
}
