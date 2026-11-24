//! Rhai, running `scripts/rp.rhai`.

use crate::host::{self, Script, Shared};
use rhai::{AST, CallFnOptions, Dynamic, FuncArgs, Module, Scope};

const SOURCE: &str = include_str!("../../scripts/rp.rhai");

#[derive(Debug)]
pub struct Engine {
    engine: rhai::Engine,
    ast: AST,
    scope: Scope<'static>,
    capped: bool,
}

impl Engine {
    fn call(&mut self, name: &str, args: impl FuncArgs) -> Result<Dynamic, String> {
        // The script's top level ran once in `new`; calls only run the function, with the constants in scope.
        let opts = CallFnOptions::new().eval_ast(false).rewind_scope(true);
        self.engine
            .call_fn_with_options(opts, &mut self.scope, &self.ast, name, args)
            .map_err(|e| e.to_string())
    }

    fn int(&mut self, name: &str, args: impl FuncArgs) -> Result<i64, String> {
        self.call(name, args)?
            .as_int()
            .map_err(|t| format!("expected an integer, got {t}"))
    }

    fn string(&mut self, name: &str, args: impl FuncArgs) -> Result<String, String> {
        self.call(name, args)?
            .into_string()
            .map_err(|t| format!("expected a string, got {t}"))
    }
}

impl Script for Engine {
    fn new(world: Shared, limits: bool) -> Result<Self, String> {
        let mut engine = rhai::Engine::new();
        engine.register_fn("host_add", host::host_add);
        let w = world.clone();
        engine.register_fn("get_money", move |id: i64| w.borrow().get_money(id));
        let w = world.clone();
        engine.register_fn("add_money", move |id: i64, d: i64| {
            w.borrow_mut().add_money(id, d)
        });
        let w = world.clone();
        engine.register_fn("player_count", move || w.borrow().player_count());
        let w = world;
        engine.register_fn("player_job", move |id: i64| w.borrow().player_job(id));
        if limits {
            engine.on_progress(|_| {
                if host::stopped() {
                    Some(Dynamic::from(host::DEADLINE_MESSAGE))
                } else {
                    None
                }
            });
            // Rhai has no cap on total memory, only on the size of each string, array and map.
            engine.set_max_string_size(1 << 20);
            engine.set_max_array_size(1 << 16);
            engine.set_max_map_size(1 << 16);
        }
        let ast = engine.compile(SOURCE).map_err(|e| e.to_string())?;
        let mut scope = Scope::new();
        engine
            .run_ast_with_scope(&mut scope, &ast)
            .map_err(|e| e.to_string())?;
        // Functions cannot read the script's top-level constants, so they are offered as the module `rp`.
        let mut module = Module::new();
        for (name, _, value) in scope.iter() {
            module.set_var(name, value);
        }
        engine.register_static_module("rp", module.into());
        Ok(Engine {
            engine,
            ast,
            scope,
            capped: false,
        })
    }

    fn noop(&mut self, x: i64) -> Result<i64, String> {
        self.int("noop", (x,))
    }

    fn host_loop(&mut self, n: i64) -> Result<i64, String> {
        self.int("host_loop", (n,))
    }

    fn echo(&mut self, s: &str) -> Result<String, String> {
        self.string("echo", (s.to_string(),))
    }

    fn zone(&mut self, x: f64, y: f64) -> Result<i64, String> {
        self.int("zone", (x, y))
    }

    fn chat(&mut self, from: i64, line: &str) -> Result<String, String> {
        self.string("chat", (from, line.to_string()))
    }

    fn payday(&mut self) -> Result<i64, String> {
        self.int("payday", ())
    }

    fn inventory(&mut self, seed: i64) -> Result<i64, String> {
        self.int("inventory", (seed,))
    }

    fn misbehave(&mut self, case: &str) -> Result<(), String> {
        let args: (i64,) = (0,);
        match case {
            "recurse" => self.call(case, args).map(|_| ()),
            _ => self.call(case, ()).map(|_| ()),
        }
    }

    fn memory_capped(&self) -> bool {
        self.capped
    }
}
