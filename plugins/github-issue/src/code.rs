use rquickjs::{Context, Ctx, Error, Function, Object, Runtime, Value};
use serde_json::{json, Value as Json};
use std::io::{self, BufRead, Read, Write};

const MAX_FRAME: usize = 96 * 1024;
const MAX_RESULT: usize = 16 * 1024;

pub(crate) fn run() -> io::Result<()> {
    if receive()? != json!({"version": 1}) {
        return Err(io::ErrorKind::InvalidData.into());
    }
    send(&json!({"version": 1}))?;
    let request = receive()?;
    let result = request["code"]
        .as_str()
        .filter(|code| code.len() <= 8 * 1024)
        .and_then(|code| evaluate(code).ok());
    match result {
        Some(value) => send(&json!({"result": value})),
        None => send(&json!({"failed": true})),
    }
}

fn evaluate(code: &str) -> rquickjs::Result<Json> {
    let runtime = Runtime::new()?;
    runtime.set_memory_limit(16 * 1024 * 1024);
    runtime.set_max_stack_size(256 * 1024);
    Context::full(&runtime)?.with(|ctx| {
        let github = Object::new(ctx.clone())?;
        let issue = Object::new(ctx.clone())?;
        issue.set("read", Function::new(ctx.clone(), read_issue)?)?;
        github.set("issue", issue)?;
        let compile: Function =
            ctx.eval("(code) => new Function('github', '\"use strict\";\\n' + code)")?;
        let script: Function = compile.call((code,))?;
        let value: Value = script.call((github,))?;
        if value.is_promise() {
            return Err(Error::Unknown);
        }
        let result = ctx
            .json_stringify(value)?
            .ok_or(Error::Unknown)?
            .to_string()?;
        if result.len() > MAX_RESULT {
            return Err(Error::Unknown);
        }
        serde_json::from_str(&result).map_err(|_| Error::Unknown)
    })
}

fn read_issue<'js>(ctx: Ctx<'js>, value: Value<'js>) -> rquickjs::Result<Value<'js>> {
    let number = value
        .as_number()
        .filter(|n| n.fract() == 0.0 && (1.0..=9_007_199_254_740_991.0).contains(n))
        .ok_or(Error::Unknown)?;
    send(&json!({"read": number as u64})).map_err(|_| Error::Unknown)?;
    let result = receive().map_err(|_| Error::Unknown)?;
    ctx.json_parse(result.to_string())
}

fn receive() -> io::Result<Json> {
    let mut bytes = Vec::new();
    io::stdin()
        .lock()
        .take(MAX_FRAME as u64 + 1)
        .read_until(b'\n', &mut bytes)?;
    if bytes.len() > MAX_FRAME || bytes.last() != Some(&b'\n') {
        return Err(io::ErrorKind::InvalidData.into());
    }
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

fn send(value: &Json) -> io::Result<()> {
    let mut out = io::stdout().lock();
    serde_json::to_writer(&mut out, value)?;
    writeln!(out)?;
    out.flush()
}
