//! Luau, Lua 5.4 and LuaJIT through mlua, all running `scripts/rp.lua`.

use crate::host::{self, MEMORY_LIMIT, Script, Shared};
use mlua::{Function, Lua, VmState};

const SOURCE: &str = include_str!("../../scripts/rp.lua");

#[derive(Debug)]
pub struct Engine {
    lua: Lua,
    noop: Function,
    host_loop: Function,
    echo: Function,
    zone: Function,
    chat: Function,
    payday: Function,
    inventory: Function,
    capped: bool,
}

fn text(e: mlua::Error) -> String {
    e.to_string()
}

fn deadline_check() -> mlua::Result<VmState> {
    if host::stopped() {
        Err(mlua::Error::runtime(host::DEADLINE_MESSAGE))
    } else {
        Ok(VmState::Continue)
    }
}

impl Script for Engine {
    fn new(world: Shared, limits: bool) -> Result<Self, String> {
        let lua = Lua::new();
        let g = lua.globals();
        g.set(
            "host_add",
            lua.create_function(|_, x: i64| Ok(host::host_add(x)))
                .map_err(text)?,
        )
        .map_err(text)?;
        let w = world.clone();
        g.set(
            "get_money",
            lua.create_function(move |_, id: i64| Ok(w.borrow().get_money(id)))
                .map_err(text)?,
        )
        .map_err(text)?;
        let w = world.clone();
        g.set(
            "add_money",
            lua.create_function(move |_, (id, d): (i64, i64)| Ok(w.borrow_mut().add_money(id, d)))
                .map_err(text)?,
        )
        .map_err(text)?;
        let w = world.clone();
        g.set(
            "player_count",
            lua.create_function(move |_, ()| Ok(w.borrow().player_count()))
                .map_err(text)?,
        )
        .map_err(text)?;
        let w = world;
        g.set(
            "player_job",
            lua.create_function(move |_, id: i64| Ok(w.borrow().player_job(id)))
                .map_err(text)?,
        )
        .map_err(text)?;

        let mut capped = false;
        if limits {
            // mlua counts allocations in its allocator; where it cannot, it reports an error and there is no cap.
            capped = lua.set_memory_limit(MEMORY_LIMIT).is_ok();
            #[cfg(feature = "luau")]
            lua.set_interrupt(|_| deadline_check());
            #[cfg(not(feature = "luau"))]
            lua.set_hook(mlua::HookTriggers::new().every_nth_instruction(1000), |_, _| {
                deadline_check()
            })
            .map_err(text)?;
        }
        // Luau's highest optimization level (inlining, loop unrolling); the default is 1.
        #[cfg(feature = "luau")]
        lua.set_compiler(mlua::chunk::Compiler::new().set_optimization_level(2));
        // The LuaJIT interpreter alone: hooks are not called from compiled code, so a JIT-compiled loop cannot be
        // stopped by the deadline hook.
        #[cfg(feature = "luajit-interp")]
        lua.load("jit.off()").exec().map_err(text)?;
        lua.load(SOURCE).set_name("rp").exec().map_err(text)?;
        // Luau's sandbox: read-only built-in libraries and globals, as Roblox runs untrusted code.
        #[cfg(feature = "luau")]
        lua.sandbox(true).map_err(text)?;
        let g = lua.globals();
        let f = |name: &str| g.get::<Function>(name).map_err(text);
        Ok(Engine {
            noop: f("noop")?,
            host_loop: f("host_loop")?,
            echo: f("echo")?,
            zone: f("zone")?,
            chat: f("chat")?,
            payday: f("payday")?,
            inventory: f("inventory")?,
            capped,
            lua,
        })
    }

    fn noop(&mut self, x: i64) -> Result<i64, String> {
        self.noop.call(x).map_err(text)
    }

    fn host_loop(&mut self, n: i64) -> Result<i64, String> {
        self.host_loop.call(n).map_err(text)
    }

    fn echo(&mut self, s: &str) -> Result<String, String> {
        self.echo.call(s).map_err(text)
    }

    fn zone(&mut self, x: f64, y: f64) -> Result<i64, String> {
        self.zone.call((x, y)).map_err(text)
    }

    fn chat(&mut self, from: i64, line: &str) -> Result<String, String> {
        self.chat.call((from, line)).map_err(text)
    }

    fn payday(&mut self) -> Result<i64, String> {
        self.payday.call(()).map_err(text)
    }

    fn inventory(&mut self, seed: i64) -> Result<i64, String> {
        self.inventory.call(seed).map_err(text)
    }

    fn misbehave(&mut self, case: &str) -> Result<(), String> {
        let f: Function = self.lua.globals().get(case).map_err(text)?;
        f.call::<()>(0).map_err(text)
    }

    fn memory_capped(&self) -> bool {
        self.capped
    }
}
