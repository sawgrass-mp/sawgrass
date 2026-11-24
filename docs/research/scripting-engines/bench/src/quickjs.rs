//! QuickJS through rquickjs, running `scripts/rp.js`.

use crate::host::{self, MEMORY_LIMIT, Script, Shared};
use rquickjs::{CatchResultExt, Context, Ctx, Function, Persistent, Runtime};

const SOURCE: &str = include_str!("../../scripts/rp.js");

#[allow(missing_debug_implementations)]
pub struct Engine {
    // Fields drop in order: the saved functions before the context, the context before the runtime.
    noop: Persistent<Function<'static>>,
    host_loop: Persistent<Function<'static>>,
    echo: Persistent<Function<'static>>,
    zone: Persistent<Function<'static>>,
    chat: Persistent<Function<'static>>,
    payday: Persistent<Function<'static>>,
    inventory: Persistent<Function<'static>>,
    ctx: Context,
    _rt: Runtime,
    capped: bool,
}

fn caught<T>(ctx: &Ctx<'_>, r: rquickjs::Result<T>) -> Result<T, String> {
    r.catch(ctx).map_err(|e| e.to_string())
}

impl Engine {
    fn call<A, R>(&self, f: &Persistent<Function<'static>>, args: A) -> Result<R, String>
    where
        A: for<'js> rquickjs::function::IntoArgs<'js>,
        R: for<'js> rquickjs::FromJs<'js>,
    {
        self.ctx.with(|ctx| {
            let f = caught(&ctx, f.clone().restore(&ctx))?;
            caught(&ctx, f.call::<A, R>(args))
        })
    }
}

impl Script for Engine {
    fn new(world: Shared, limits: bool) -> Result<Self, String> {
        let rt = Runtime::new().map_err(|e| e.to_string())?;
        if limits {
            rt.set_memory_limit(MEMORY_LIMIT);
            rt.set_interrupt_handler(Some(Box::new(host::stopped)));
        }
        let ctx = Context::full(&rt).map_err(|e| e.to_string())?;
        let fns = ctx.with(|ctx| -> rquickjs::Result<_> {
            let g = ctx.globals();
            // JavaScript numbers are doubles; the host functions take and return f64 and convert.
            g.set(
                "host_add",
                Function::new(ctx.clone(), |x: f64| host::host_add(x as i64) as f64)?,
            )?;
            let w = world.clone();
            g.set(
                "get_money",
                Function::new(ctx.clone(), move |id: f64| w.borrow().get_money(id as i64) as f64)?,
            )?;
            let w = world.clone();
            g.set(
                "add_money",
                Function::new(ctx.clone(), move |id: f64, d: f64| {
                    w.borrow_mut().add_money(id as i64, d as i64)
                })?,
            )?;
            let w = world.clone();
            g.set(
                "player_count",
                Function::new(ctx.clone(), move || w.borrow().player_count() as f64)?,
            )?;
            let w = world.clone();
            g.set(
                "player_job",
                Function::new(ctx.clone(), move |id: f64| {
                    w.borrow().player_job(id as i64) as f64
                })?,
            )?;
            ctx.eval::<(), _>(SOURCE)?;
            let f = |name: &str| -> rquickjs::Result<Persistent<Function<'static>>> {
                Ok(Persistent::save(&ctx, g.get::<_, Function>(name)?))
            };
            Ok([
                f("noop")?,
                f("host_loop")?,
                f("echo")?,
                f("zone")?,
                f("chat")?,
                f("payday")?,
                f("inventory")?,
            ])
        });
        let [noop, host_loop, echo, zone, chat, payday, inventory] = fns.map_err(|e| e.to_string())?;
        Ok(Engine {
            _rt: rt,
            ctx,
            noop,
            host_loop,
            echo,
            zone,
            chat,
            payday,
            inventory,
            capped: limits,
        })
    }

    fn noop(&mut self, x: i64) -> Result<i64, String> {
        self.call::<_, f64>(&self.noop, (x as f64,)).map(|v| v as i64)
    }

    fn host_loop(&mut self, n: i64) -> Result<i64, String> {
        self.call::<_, f64>(&self.host_loop, (n as f64,))
            .map(|v| v as i64)
    }

    fn echo(&mut self, s: &str) -> Result<String, String> {
        self.call(&self.echo, (s,))
    }

    fn zone(&mut self, x: f64, y: f64) -> Result<i64, String> {
        self.call::<_, f64>(&self.zone, (x, y)).map(|v| v as i64)
    }

    fn chat(&mut self, from: i64, line: &str) -> Result<String, String> {
        self.call(&self.chat, (from as f64, line))
    }

    fn payday(&mut self) -> Result<i64, String> {
        self.call::<_, f64>(&self.payday, ()).map(|v| v as i64)
    }

    fn inventory(&mut self, seed: i64) -> Result<i64, String> {
        self.call::<_, f64>(&self.inventory, (seed as f64,))
            .map(|v| v as i64)
    }

    fn misbehave(&mut self, case: &str) -> Result<(), String> {
        self.ctx.with(|ctx| {
            let f: Function = caught(&ctx, ctx.globals().get(case))?;
            caught(&ctx, f.call::<_, ()>((0,)))
        })
    }

    fn memory_capped(&self) -> bool {
        self.capped
    }
}
