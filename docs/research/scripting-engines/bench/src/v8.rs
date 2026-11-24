//! V8 through the `v8` crate (rusty_v8), running `scripts/rp.js`. One isolate per instance, since limits (heap,
//! termination) apply per isolate.

use crate::host::{self, MEMORY_LIMIT, Script, Shared};
use std::cell::RefCell;
use std::ffi::c_void;
use std::sync::Once;

const SOURCE: &str = include_str!("../../scripts/rp.js");
const FUNCTIONS: [&str; 11] = [
    "noop",
    "host_loop",
    "echo",
    "zone",
    "chat",
    "payday",
    "inventory",
    "spin",
    "bomb",
    "recurse",
    "fail",
];

static INIT: Once = Once::new();

thread_local! {
    /// The isolate whose heap limit callback may fire, for the callback to terminate it.
    static CURRENT: RefCell<Option<v8::IsolateHandle>> = const { RefCell::new(None) };
}

/// Called by V8 when the heap nears its limit. Without it V8 aborts the whole process when an isolate runs out of
/// heap. It terminates the running script and raises the limit so that the termination can unwind, as Deno does.
/// Declared `unsafe extern "C"` because that is the callback type; it performs no unsafe operation.
#[allow(unsafe_code)]
unsafe extern "C" fn near_heap_limit(_data: *mut c_void, current: usize, _initial: usize) -> usize {
    CURRENT.with_borrow(|h| {
        if let Some(h) = h {
            h.terminate_execution();
        }
    });
    current * 2
}

#[derive(Debug)]
pub struct Engine {
    isolate: v8::OwnedIsolate,
    context: v8::Global<v8::Context>,
    fns: Vec<v8::Global<v8::Function>>,
    limits: bool,
}

enum Arg<'a> {
    Num(f64),
    Str(&'a str),
}

enum Out {
    Num(f64),
    Str(String),
}

fn world(scope: &mut v8::PinScope) -> Shared {
    scope.get_slot::<Shared>().cloned().unwrap_or_default()
}

fn num(scope: &mut v8::PinScope, args: &v8::FunctionCallbackArguments, i: i32) -> i64 {
    args.get(i).number_value(scope).unwrap_or(0.0) as i64
}

fn host_add(scope: &mut v8::PinScope, args: v8::FunctionCallbackArguments, mut rv: v8::ReturnValue) {
    rv.set_double(host::host_add(num(scope, &args, 0)) as f64);
}

fn get_money(scope: &mut v8::PinScope, args: v8::FunctionCallbackArguments, mut rv: v8::ReturnValue) {
    let id = num(scope, &args, 0);
    rv.set_double(world(scope).borrow().get_money(id) as f64);
}

fn add_money(scope: &mut v8::PinScope, args: v8::FunctionCallbackArguments, mut rv: v8::ReturnValue) {
    let (id, d) = (num(scope, &args, 0), num(scope, &args, 1));
    rv.set_bool(world(scope).borrow_mut().add_money(id, d));
}

fn player_count(scope: &mut v8::PinScope, _args: v8::FunctionCallbackArguments, mut rv: v8::ReturnValue) {
    rv.set_double(world(scope).borrow().player_count() as f64);
}

fn player_job(scope: &mut v8::PinScope, args: v8::FunctionCallbackArguments, mut rv: v8::ReturnValue) {
    let id = num(scope, &args, 0);
    rv.set_double(world(scope).borrow().player_job(id) as f64);
}

impl Engine {
    fn call(&mut self, index: usize, args: &[Arg<'_>]) -> Result<Out, String> {
        let isolate = &mut self.isolate;
        v8::scope!(let hs, isolate);
        let context = v8::Local::new(hs, &self.context);
        let cs = &mut v8::ContextScope::new(hs, context);
        v8::tc_scope!(let tc, cs);
        let f = v8::Local::new(tc, &self.fns[index]);
        let mut argv: Vec<v8::Local<v8::Value>> = Vec::with_capacity(args.len());
        for a in args {
            argv.push(match a {
                Arg::Num(n) => v8::Number::new(tc, *n).into(),
                Arg::Str(s) => v8::String::new(tc, s).ok_or("string too long")?.into(),
            });
        }
        let recv = v8::undefined(tc).into();
        match f.call(tc, recv, &argv) {
            Some(v) if v.is_string() => Ok(Out::Str(v.to_rust_string_lossy(tc))),
            Some(v) => Ok(Out::Num(v.number_value(tc).unwrap_or(f64::NAN))),
            None if tc.has_terminated() => Err("terminated".into()),
            None => Err(tc
                .exception()
                .and_then(|e| e.to_string(tc))
                .map(|s| s.to_rust_string_lossy(tc))
                .unwrap_or_else(|| "exception".into())),
        }
    }

    fn num(&mut self, index: usize, args: &[Arg<'_>]) -> Result<i64, String> {
        match self.call(index, args)? {
            Out::Num(n) => Ok(n as i64),
            Out::Str(s) => Err(format!("expected a number, got {s:?}")),
        }
    }

    fn string(&mut self, index: usize, args: &[Arg<'_>]) -> Result<String, String> {
        match self.call(index, args)? {
            Out::Str(s) => Ok(s),
            Out::Num(n) => Err(format!("expected a string, got {n}")),
        }
    }
}

impl Script for Engine {
    fn new(world: Shared, limits: bool) -> Result<Self, String> {
        INIT.call_once(|| {
            v8::V8::initialize_platform(v8::new_default_platform(0, false).make_shared());
            v8::V8::initialize();
        });
        let mut params = v8::CreateParams::default();
        if limits {
            params = params.heap_limits(0, MEMORY_LIMIT);
        }
        let mut isolate = v8::Isolate::new(params);
        isolate.set_slot(world);
        if limits {
            isolate.add_near_heap_limit_callback(near_heap_limit, std::ptr::null_mut());
            let handle = isolate.thread_safe_handle();
            host::on_expire(move || {
                handle.terminate_execution();
            });
        }
        let (context, fns) = {
            let isolate = &mut isolate;
            v8::scope!(let hs, isolate);
            let global = v8::ObjectTemplate::new(hs);
            for (name, f) in [
                ("host_add", v8::FunctionTemplate::new(hs, host_add)),
                ("get_money", v8::FunctionTemplate::new(hs, get_money)),
                ("add_money", v8::FunctionTemplate::new(hs, add_money)),
                ("player_count", v8::FunctionTemplate::new(hs, player_count)),
                ("player_job", v8::FunctionTemplate::new(hs, player_job)),
            ] {
                let key = v8::String::new(hs, name).ok_or("name")?;
                global.set(key.into(), f.into());
            }
            let context = v8::Context::new(
                hs,
                v8::ContextOptions {
                    global_template: Some(global),
                    ..Default::default()
                },
            );
            let cs = &mut v8::ContextScope::new(hs, context);
            v8::tc_scope!(let tc, cs);
            let code = v8::String::new(tc, SOURCE).ok_or("source")?;
            let script = v8::Script::compile(tc, code, None).ok_or("compile error")?;
            script.run(tc).ok_or("script error")?;
            let g = context.global(tc);
            let mut fns = Vec::new();
            for name in FUNCTIONS {
                let key = v8::String::new(tc, name).ok_or("name")?;
                let f = g.get(tc, key.into()).ok_or("missing function")?;
                let f = v8::Local::<v8::Function>::try_from(f).map_err(|e| e.to_string())?;
                fns.push(v8::Global::new(tc, f));
            }
            (v8::Global::new(tc, context), fns)
        };
        Ok(Engine {
            isolate,
            context,
            fns,
            limits,
        })
    }

    fn noop(&mut self, x: i64) -> Result<i64, String> {
        self.num(0, &[Arg::Num(x as f64)])
    }

    fn host_loop(&mut self, n: i64) -> Result<i64, String> {
        self.num(1, &[Arg::Num(n as f64)])
    }

    fn echo(&mut self, s: &str) -> Result<String, String> {
        self.string(2, &[Arg::Str(s)])
    }

    fn zone(&mut self, x: f64, y: f64) -> Result<i64, String> {
        self.num(3, &[Arg::Num(x), Arg::Num(y)])
    }

    fn chat(&mut self, from: i64, line: &str) -> Result<String, String> {
        self.string(4, &[Arg::Num(from as f64), Arg::Str(line)])
    }

    fn payday(&mut self) -> Result<i64, String> {
        self.num(5, &[])
    }

    fn inventory(&mut self, seed: i64) -> Result<i64, String> {
        self.num(6, &[Arg::Num(seed as f64)])
    }

    fn misbehave(&mut self, case: &str) -> Result<(), String> {
        let index = FUNCTIONS.iter().position(|n| *n == case).ok_or("unknown case")?;
        CURRENT.set(Some(self.isolate.thread_safe_handle()));
        let r = self.call(index, &[Arg::Num(0.0)]).map(|_| ());
        CURRENT.set(None);
        r
    }

    fn recover(&mut self) {
        self.isolate.cancel_terminate_execution();
    }

    fn memory_capped(&self) -> bool {
        self.limits
    }
}
