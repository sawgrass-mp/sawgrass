//! Guards for the built-in functions Luau does not interrupt (ADR 0005, Consequences).
//!
//! Luau's interrupt callback is not called inside `table.sort` with its default comparison, `string.rep`,
//! `table.concat`, or any other single built-in that copies or fills memory (`..`, `buffer.create`), so the
//! deadline cannot stop them mid-call; it stops the script at the next loop or call after them. The bench
//! (`cargo bench -p sawgrass-script`) measures them:
//!
//! - `table.sort` with the default comparison grows with the table, up to seconds at the memory cap. Above
//!   [`SORT_UNGUARDED`] elements it now sorts with a Luau comparison, which the interrupt reaches, so the deadline
//!   stops it. Same order, slower; at the guard's size the built-in sort takes about 0.4 ms.
//! - Building one large string or buffer costs about 1 ms per MiB of result (52 ms for a 48 MiB `string.rep`,
//!   17 ms to `table.concat` 24 MiB). The memory cap is the only bound on one such call, so the worst single stall
//!   is about 1 ms per MiB of [`crate::Limits::memory`]. Wrapping `string.rep` and `table.concat` alone would not
//!   change that bound, because `a .. b` cannot be wrapped. [`crate::Stats::slowest`] shows such a stall.

use mlua::Lua;

/// The most elements `table.sort` sorts with its built-in comparison.
pub const SORT_UNGUARDED: usize = 4096;

pub(crate) fn install(lua: &Lua) -> mlua::Result<()> {
    lua.load(format!(
        r"
        local sort = table.sort
        local function less(a, b) return a < b end
        table.sort = function(t, comp)
            if comp == nil and #t > {SORT_UNGUARDED} then
                comp = less
            end
            return sort(t, comp)
        end
        "
    ))
    .set_name("=guards")
    .exec()
}
