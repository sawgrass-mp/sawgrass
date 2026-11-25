//! Guards for the built-in functions Luau does not interrupt (ADR 0005, Consequences).
//!
//! Luau's interrupt callback is not called inside `table.sort` with its default comparison, `string.rep`,
//! `table.concat`, or any other single built-in that copies or fills memory (`..`, `buffer.create`), so the
//! deadline cannot stop them mid-call; it stops the script at the next loop or call after them. The bench
//! (`cargo bench -p sawgrass-script`) measures them:
//!
//! - `table.sort` runs its comparison from C, so a C function as the comparison (`table.sort(t, rawequal)`, or
//!   none, which uses the built-in one) never reaches the interrupt: 0.55 s for a million numbers. Above
//!   [`SORT_UNGUARDED`] elements every comparison now runs inside a Luau function, which the interrupt reaches, so
//!   the deadline stops the sort. Same order, slower; at the guard's size the built-in sort takes about 0.4 ms.
//! - `table.foreach` and `table.foreachi` call their function from C in the same way: 30 to 60 ms over a million
//!   elements with a C function such as `math.randomseed`. They always call it through a Luau function now.
//! - The other built-ins that take a function (`string.gsub`, `pcall`, `xpcall`, `coroutine.wrap`) were checked:
//!   the pattern matcher reaches the interrupt, and the others make one call, so the deadline stops them.
//! - Building one large string or buffer costs about 1 ms per MiB of result (52 ms for a 48 MiB `string.rep`,
//!   17 ms to `table.concat` 24 MiB). The memory cap is the only bound on one such call, so the worst single stall
//!   is about 1 ms per MiB of [`crate::Limits::memory`]. Wrapping `string.rep` and `table.concat` alone would not
//!   change that bound, because `a .. b` cannot be wrapped. [`crate::Stats::slowest`] shows such a stall.

use mlua::Lua;

/// The most elements `table.sort` sorts calling its comparison directly from C.
pub const SORT_UNGUARDED: usize = 4096;

pub(crate) fn install(lua: &Lua) -> mlua::Result<()> {
    lua.load(format!(
        r"
        local sort, foreach, foreachi = table.sort, table.foreach, table.foreachi
        local function less(a, b) return a < b end
        table.sort = function(t, comp)
            if #t > {SORT_UNGUARDED} then
                if comp == nil then
                    comp = less
                else
                    local c = comp
                    comp = function(a, b) return c(a, b) end
                end
            end
            return sort(t, comp)
        end
        table.foreach = function(t, f)
            return foreach(t, function(k, v) return f(k, v) end)
        end
        table.foreachi = function(t, f)
            return foreachi(t, function(i, v) return f(i, v) end)
        end
        "
    ))
    .set_name("=guards")
    .exec()
}
