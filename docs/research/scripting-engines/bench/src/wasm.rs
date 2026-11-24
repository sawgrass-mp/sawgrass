//! WebAssembly through wasmtime, running the guest in `../guest` (Rust compiled to wasm32-unknown-unknown).

use crate::host::{self, MEMORY_LIMIT, Script, Shared, World};
use std::sync::OnceLock;
use std::time::Instant;
use wasmtime::{
    Caller, Config, InstancePre, Linker, Memory, Module, Store, StoreLimits, StoreLimitsBuilder, TypedFunc,
};

const GUEST: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../guest/target/wasm32-unknown-unknown/release/rp_guest.wasm"
);

#[derive(Debug)]
pub struct Data {
    world: World,
    limits: StoreLimits,
}

struct Shared_ {
    engine: wasmtime::Engine,
    pre: InstancePre<Data>,
    compile_ms: f64,
}

static SHARED: OnceLock<Shared_> = OnceLock::new();

/// Compiles the guest once per process, as a server would at resource start; instances are made from it.
fn shared(limits: bool) -> Result<&'static Shared_, String> {
    if let Some(s) = SHARED.get() {
        return Ok(s);
    }
    let mut config = Config::new();
    config.epoch_interruption(limits);
    let engine = wasmtime::Engine::new(&config).map_err(|e| e.to_string())?;
    let bytes = std::fs::read(GUEST).map_err(|e| format!("{GUEST}: {e} (build ../guest first)"))?;
    let t = Instant::now();
    let module = Module::new(&engine, &bytes).map_err(|e| e.to_string())?;
    let compile_ms = t.elapsed().as_secs_f64() * 1e3;
    let mut linker: Linker<Data> = Linker::new(&engine);
    let e = |r: wasmtime::Result<&mut Linker<Data>>| r.map(|_| ()).map_err(|e| e.to_string());
    e(linker.func_wrap("env", "host_add", host::host_add))?;
    e(
        linker.func_wrap("env", "get_money", |c: Caller<'_, Data>, id: i64| {
            c.data().world.get_money(id)
        }),
    )?;
    e(
        linker.func_wrap("env", "add_money", |mut c: Caller<'_, Data>, id: i64, d: i64| {
            c.data_mut().world.add_money(id, d) as i32
        }),
    )?;
    e(linker.func_wrap("env", "player_count", |c: Caller<'_, Data>| {
        c.data().world.player_count()
    }))?;
    e(
        linker.func_wrap("env", "player_job", |c: Caller<'_, Data>, id: i64| {
            c.data().world.player_job(id)
        }),
    )?;
    let pre = linker.instantiate_pre(&module).map_err(|e| e.to_string())?;
    if limits {
        let kick = engine.clone();
        host::on_expire(move || kick.increment_epoch());
    }
    Ok(SHARED.get_or_init(|| Shared_ {
        engine,
        pre,
        compile_ms,
    }))
}

pub fn compile_ms() -> Option<f64> {
    SHARED.get().map(|s| s.compile_ms)
}

#[allow(missing_debug_implementations)]
pub struct Engine {
    store: Store<Data>,
    memory: Memory,
    limits: bool,
    buf_in: TypedFunc<i32, i32>,
    buf_out: TypedFunc<(), i32>,
    noop: TypedFunc<i64, i64>,
    host_loop: TypedFunc<i64, i64>,
    echo: TypedFunc<(), i32>,
    zone: TypedFunc<(f64, f64), i64>,
    chat: TypedFunc<i64, i32>,
    payday: TypedFunc<(), i64>,
    inventory: TypedFunc<i64, i64>,
    instance: wasmtime::Instance,
}

fn text(e: wasmtime::Error) -> String {
    match e.downcast_ref::<wasmtime::Trap>() {
        Some(trap) => format!("trap: {trap}"),
        None => e.root_cause().to_string(),
    }
}

impl Engine {
    /// Gives the next call a deadline: it traps once the watchdog advances the epoch.
    fn arm(&mut self) {
        if self.limits {
            self.store.set_epoch_deadline(1);
        }
    }

    fn put(&mut self, s: &str) -> Result<(), String> {
        let ptr = self.buf_in.call(&mut self.store, s.len() as i32).map_err(text)?;
        self.memory
            .write(&mut self.store, ptr as usize, s.as_bytes())
            .map_err(|e| e.to_string())
    }

    fn take(&mut self, len: i32) -> Result<String, String> {
        let ptr = self.buf_out.call(&mut self.store, ()).map_err(text)? as usize;
        let bytes = self
            .memory
            .data(&self.store)
            .get(ptr..ptr + len as usize)
            .ok_or("reply out of bounds")?;
        String::from_utf8(bytes.to_vec()).map_err(|e| e.to_string())
    }
}

impl Script for Engine {
    fn new(world: Shared, limits: bool) -> Result<Self, String> {
        let s = shared(limits)?;
        let world = std::mem::take(&mut *world.borrow_mut());
        let mut b = StoreLimitsBuilder::new();
        if limits {
            b = b.memory_size(MEMORY_LIMIT);
        }
        let mut store = Store::new(
            &s.engine,
            Data {
                world,
                limits: b.build(),
            },
        );
        store.limiter(|d| &mut d.limits);
        if limits {
            store.set_epoch_deadline(1);
        }
        let instance = s.pre.instantiate(&mut store).map_err(text)?;
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or("no memory export")?;
        macro_rules! f {
            ($name:literal) => {
                instance.get_typed_func(&mut store, $name).map_err(text)?
            };
        }
        Ok(Engine {
            buf_in: f!("buf_in"),
            buf_out: f!("buf_out"),
            noop: f!("noop"),
            host_loop: f!("host_loop"),
            echo: f!("echo"),
            zone: f!("zone"),
            chat: f!("chat"),
            payday: f!("payday"),
            inventory: f!("inventory"),
            memory,
            instance,
            store,
            limits,
        })
    }

    fn noop(&mut self, x: i64) -> Result<i64, String> {
        self.arm();
        self.noop.call(&mut self.store, x).map_err(text)
    }

    fn host_loop(&mut self, n: i64) -> Result<i64, String> {
        self.arm();
        self.host_loop.call(&mut self.store, n).map_err(text)
    }

    fn echo(&mut self, s: &str) -> Result<String, String> {
        self.arm();
        self.put(s)?;
        let len = self.echo.call(&mut self.store, ()).map_err(text)?;
        self.take(len)
    }

    fn zone(&mut self, x: f64, y: f64) -> Result<i64, String> {
        self.arm();
        self.zone.call(&mut self.store, (x, y)).map_err(text)
    }

    fn chat(&mut self, from: i64, line: &str) -> Result<String, String> {
        self.arm();
        self.put(line)?;
        let len = self.chat.call(&mut self.store, from).map_err(text)?;
        self.take(len)
    }

    fn payday(&mut self) -> Result<i64, String> {
        self.arm();
        self.payday.call(&mut self.store, ()).map_err(text)
    }

    fn inventory(&mut self, seed: i64) -> Result<i64, String> {
        self.arm();
        self.inventory.call(&mut self.store, seed).map_err(text)
    }

    fn misbehave(&mut self, case: &str) -> Result<(), String> {
        self.arm();
        if case == "recurse" {
            let f: TypedFunc<i64, i64> = self
                .instance
                .get_typed_func(&mut self.store, case)
                .map_err(text)?;
            f.call(&mut self.store, 0).map(|_| ()).map_err(text)
        } else {
            let f: TypedFunc<(), ()> = self
                .instance
                .get_typed_func(&mut self.store, case)
                .map_err(text)?;
            f.call(&mut self.store, ()).map_err(text)
        }
    }

    fn memory_capped(&self) -> bool {
        self.limits
    }

    /// The store owns the world here; the checksum is read from it.
    fn checksum(&self, _world: &Shared) -> i64 {
        self.store.data().world.checksum()
    }
}
