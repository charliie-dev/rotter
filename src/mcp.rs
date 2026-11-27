//! `rotter mcp`: a Model Context Protocol server on stdio, protocol version 2026-07-28 only.
//!
//! The protocol is stateless: there is no `initialize` handshake, and every request carries its
//! protocol version and client capabilities in `_meta`. The server offers one read-only tool,
//! `extract` (the CLI's `rotter extract`, run through the same code), and one prompt, the comment
//! review skill. Messages are newline-delimited JSON-RPC 2.0 on stdin and stdout; stdout carries
//! nothing else. Once stdin closes, the server answers the requests it already read and exits.

use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};

pub const PROTOCOL_VERSION: &str = "2026-07-28";

/// Nothing in discover or the lists changes while the binary runs; clients may cache them.
const TTL_MS: u64 = 3_600_000;

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const UNSUPPORTED_PROTOCOL_VERSION: i64 = -32022;

const VERSION_KEY: &str = "io.modelcontextprotocol/protocolVersion";
const CAPABILITIES_KEY: &str = "io.modelcontextprotocol/clientCapabilities";
const SERVER_INFO_KEY: &str = "io.modelcontextprotocol/serverInfo";

const PROMPT: &str = "rotter-comment-review";

/// Runs `rotter extract` with these CLI arguments, `extract` first: the report's JSON text and
/// the notes the CLI would print on stderr. The flag is set when the request is cancelled.
pub type Extract<'a> =
    &'a dyn Fn(Vec<String>, Arc<AtomicBool>) -> Result<(String, Vec<String>), String>;

/// Requests read but not yet answered, by id key, with their cancel flags. The reader thread
/// adds each request before passing it on, so a cancellation always finds a request it follows.
type Pending = Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>;

/// A line the reader thread passed on, with the id key and cancel flag it registered when the
/// line is a request.
type Read = (Vec<u8>, Option<(String, Arc<AtomicBool>)>);

const MCP_GUIDE: &str = "You reach rotter through MCP: call the `extract` tool where the steps \
below run `rotter extract`. The mode flags become `mode` (`staged`, `worktree`, `full`, or `base` \
with the revision in `base`); `-C <dir>` becomes `directory`, `-- <pathspec>...` becomes `paths`, \
`--lang` becomes `lang` and `--include-untracked` becomes `include_untracked`. Exit status 0 is a \
result whose report has `complete: true`, exit 1 one with `complete: false`, and exit 2 a result \
with `isError: true` whose text says why. What the CLI prints on stderr comes as further text \
content after the report.";

struct RpcError(i64, String, Option<Value>);

impl RpcError {
    fn params(message: impl Into<String>) -> Self {
        Self(INVALID_PARAMS, message.into(), None)
    }
}

pub fn serve<R, W>(input: R, mut output: W, extract: Extract, skill: &str) -> std::io::Result<()>
where
    R: BufRead + Send + 'static,
    W: Write,
{
    // A reader thread keeps taking messages while a request runs, so a cancellation reaches a
    // running extract, which stops early, and a waiting request, which never runs. Lines stay
    // bytes: a line that is not UTF-8 is a parse error for that message, not the end of the stream.
    let (sender, receiver) = mpsc::channel::<std::io::Result<Read>>();
    let pending: Pending = Arc::default();
    let reader_pending = Arc::clone(&pending);
    std::thread::spawn(move || read_lines(input, &sender, &reader_pending));
    while let Ok(read) = receiver.recv() {
        let (line, request) = read?;
        let cancel = request
            .as_ref()
            .map(|(_, cancel)| Arc::clone(cancel))
            .unwrap_or_default();
        let response = if cancel.load(Ordering::Relaxed) {
            None
        } else {
            handle(&line, extract, skill, Arc::clone(&cancel))
        };
        // A later request may reuse the id once this one is cancelled; its entry stays.
        if let Some((key, cancel)) = &request {
            let mut pending = lock(&pending);
            if pending
                .get(key)
                .is_some_and(|entry| Arc::ptr_eq(entry, cancel))
            {
                pending.remove(key);
            }
        }
        let Some(response) = response else {
            continue;
        };
        if cancel.load(Ordering::Relaxed) {
            continue;
        }
        let mut text = serde_json::to_string(&response).map_err(std::io::Error::other)?;
        text.push('\n');
        output.write_all(text.as_bytes())?;
        output.flush()?;
    }
    Ok(())
}

/// Sends each line of `input`, trimmed of surrounding ASCII whitespace and blank lines left out,
/// until the input ends, a read fails or nobody receives. A cancellation is not passed on: it
/// sets the flag of the request it names when that request is still pending, and anything else
/// is ignored, as a cancellation of a request already answered, or of none, must be.
fn read_lines<R: BufRead>(
    mut input: R,
    sender: &mpsc::Sender<std::io::Result<Read>>,
    pending: &Pending,
) {
    loop {
        let mut line = Vec::new();
        let line = match input.read_until(b'\n', &mut line) {
            Ok(0) => return,
            Ok(_) => line.trim_ascii().to_vec(),
            Err(error) => {
                let _ = sender.send(Err(error));
                return;
            }
        };
        if line.is_empty() {
            continue;
        }
        if let Some(target) = cancellation(&line) {
            if let Some(cancel) = lock(pending).get(&target) {
                cancel.store(true, Ordering::Relaxed);
            }
            continue;
        }
        let request = request_key(&line).map(|key| {
            let cancel = Arc::new(AtomicBool::new(false));
            lock(pending).insert(key.clone(), Arc::clone(&cancel));
            (key, cancel)
        });
        if sender.send(Ok((line, request))).is_err() {
            return;
        }
    }
}

/// A poisoned lock only means the other thread panicked; the map is still usable.
fn lock(pending: &Pending) -> std::sync::MutexGuard<'_, HashMap<String, Arc<AtomicBool>>> {
    pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The request's `id` as a map key: `1` and `"1"` are different ids.
fn id_key(id: &Value) -> String {
    id.to_string()
}

fn is_request_id(id: &Value) -> bool {
    id.is_string() || id.is_i64() || id.is_u64()
}

fn request_key(line: &[u8]) -> Option<String> {
    let message = serde_json::from_slice::<Value>(line).ok()?;
    message["method"].as_str()?;
    message.get("id").filter(|id| is_request_id(id)).map(id_key)
}

/// The key of the request a `notifications/cancelled` names; None for any other message.
fn cancellation(line: &[u8]) -> Option<String> {
    let message = serde_json::from_slice::<Value>(line).ok()?;
    if message.get("id").is_some() || message["method"] != "notifications/cancelled" {
        return None;
    }
    message["params"].get("requestId").map(id_key)
}

/// The response to one message, or None for a notification or a response: this server sends no
/// requests, so no response is addressed to it.
fn handle(line: &[u8], extract: Extract, skill: &str, cancel: Arc<AtomicBool>) -> Option<Value> {
    let message: Value = match serde_json::from_slice(line) {
        Ok(message) => message,
        Err(error) => {
            let error = RpcError(PARSE_ERROR, format!("parse error: {error}"), None);
            return Some(error_response(None, error));
        }
    };
    let invalid = |what: &str| {
        let error = RpcError(INVALID_REQUEST, what.into(), None);
        error_response(None, error)
    };
    let Some(object) = message.as_object() else {
        return Some(invalid("a message must be one JSON object"));
    };
    let method = object.get("method").and_then(Value::as_str);
    if method.is_none() && (object.contains_key("result") || object.contains_key("error")) {
        return None;
    }
    let Some(id) = object.get("id") else {
        return match method {
            Some(_) => None,
            None => Some(invalid("expected a JSON-RPC 2.0 request or notification")),
        };
    };
    let id = Some(id).filter(|id| is_request_id(id));
    let version = object.get("jsonrpc").and_then(Value::as_str);
    let (Some(id), Some(method), Some("2.0")) = (id, method, version) else {
        return Some(error_response(
            id,
            RpcError(
                INVALID_REQUEST,
                "expected a JSON-RPC 2.0 request with a string or integer id and a method".into(),
                None,
            ),
        ));
    };
    Some(
        match dispatch(method, object.get("params"), extract, skill, cancel) {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(error) => error_response(Some(id), error),
        },
    )
}

/// An error response; without a readable request id it carries none, as the MCP schema has it.
fn error_response(id: Option<&Value>, RpcError(code, message, data): RpcError) -> Value {
    let mut error = json!({ "code": code, "message": message });
    if let Some(data) = data {
        error["data"] = data;
    }
    let mut response = json!({ "jsonrpc": "2.0", "error": error });
    if let Some(id) = id {
        response["id"] = id.clone();
    }
    response
}

fn dispatch(
    method: &str,
    params: Option<&Value>,
    extract: Extract,
    skill: &str,
    cancel: Arc<AtomicBool>,
) -> Result<Value, RpcError> {
    let known = [
        "server/discover",
        "tools/list",
        "tools/call",
        "prompts/list",
        "prompts/get",
    ];
    if method == "initialize" {
        // Legacy clients cannot fall forward; this message may be all they can show.
        return Err(RpcError(
            METHOD_NOT_FOUND,
            format!(
                "initialize is the handshake of protocol 2025-11-25 and earlier; rotter's MCP \
                 server speaks only protocol {PROTOCOL_VERSION}"
            ),
            Some(json!({ "supported": [PROTOCOL_VERSION] })),
        ));
    }
    let empty = Map::new();
    let params = match params {
        None => &empty,
        Some(Value::Object(params)) => params,
        Some(_) => return Err(RpcError::params("params must be an object")),
    };
    // The version and capabilities are checked before the method: an unsupported version gets
    // -32022 whatever it asks for.
    check_meta(params)?;
    if !known.contains(&method) {
        return Err(RpcError(
            METHOD_NOT_FOUND,
            format!("method not found: {method}"),
            None,
        ));
    }
    let result = match method {
        "server/discover" => discover(),
        "tools/list" => {
            no_cursor(params)?;
            list(json!({ "tools": [extract_tool()] }))
        }
        "tools/call" => call_tool(params, extract, cancel)?,
        "prompts/list" => {
            no_cursor(params)?;
            list(json!({ "prompts": [prompt()] }))
        }
        "prompts/get" => get_prompt(params, skill)?,
        _ => unreachable!("every known method is handled"),
    };
    Ok(complete(result))
}

fn check_meta(params: &Map<String, Value>) -> Result<(), RpcError> {
    let meta = params.get("_meta").and_then(Value::as_object);
    let version = meta
        .and_then(|meta| meta.get(VERSION_KEY))
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::params(format!("params._meta must carry \"{VERSION_KEY}\"")))?;
    if version != PROTOCOL_VERSION {
        return Err(RpcError(
            UNSUPPORTED_PROTOCOL_VERSION,
            "Unsupported protocol version".into(),
            Some(json!({ "supported": [PROTOCOL_VERSION], "requested": version })),
        ));
    }
    if !meta.is_some_and(|meta| meta.get(CAPABILITIES_KEY).is_some_and(Value::is_object)) {
        return Err(RpcError::params(format!(
            "params._meta must carry \"{CAPABILITIES_KEY}\" (an object)"
        )));
    }
    Ok(())
}

/// The lists fit in one page, so no cursor this server could have issued exists.
fn no_cursor(params: &Map<String, Value>) -> Result<(), RpcError> {
    match params.get("cursor") {
        None | Some(Value::Null) => Ok(()),
        Some(_) => Err(RpcError::params("invalid cursor: this list has one page")),
    }
}

fn complete(mut result: Value) -> Value {
    result["resultType"] = "complete".into();
    result["_meta"] = json!({
        SERVER_INFO_KEY: { "name": "rotter", "version": env!("CARGO_PKG_VERSION") }
    });
    result
}

/// A result clients may cache: nothing in it depends on the user.
fn list(mut result: Value) -> Value {
    result["ttlMs"] = TTL_MS.into();
    result["cacheScope"] = "public".into();
    result
}

fn discover() -> Value {
    list(json!({
        "supportedVersions": [PROTOCOL_VERSION],
        "capabilities": { "tools": {}, "prompts": {} },
        "instructions": "rotter extracts the code comments related to a Git change. Call \
            `extract` on the repository, then review the comments it reports with the \
            `rotter-comment-review` prompt: concrete contradictions between a comment and the \
            code it describes, and comments that say nothing the code does not.",
    }))
}

fn extract_tool() -> Value {
    json!({
        "name": "extract",
        "title": "Extract changed code and its comments",
        "description": "Runs `rotter extract`: lists the code units a Git change touches in \
            Go, Lua, Nix, Shell, YAML, TOML, Rust and enabled external languages, each with the \
            comments related to it, as a rotter.extract.poc/0 report. Read-only: it never \
            writes the repository or its index and never runs repository hooks or filters. It \
            reads only repositories whose top level is inside a root the server was started \
            with. `complete` is false when some changed file could not be analysed.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "mode": {
                    "type": "string",
                    "enum": ["staged", "worktree", "base", "full"],
                    "description": "staged: index against HEAD; worktree: working tree against \
                        HEAD; base: working tree against `base`; full: every tracked file, no diff",
                },
                "base": {
                    "type": "string",
                    "description": "The revision to compare against; required with mode base",
                },
                "directory": {
                    "type": "string",
                    "description": "A directory inside a repository under one of the server's \
                        roots (default: the server's working directory)",
                },
                "include_untracked": {
                    "type": "boolean",
                    "description": "Also analyse untracked files (not with mode staged)",
                },
                "paths": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Git pathspecs limiting the report",
                },
                "lang": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "<glob>=<language> overrides, as with --lang",
                },
            },
            "required": ["mode"],
            "additionalProperties": false,
        },
        "annotations": {
            "title": "Extract changed code and its comments",
            "readOnlyHint": true,
            "openWorldHint": false,
        },
    })
}

fn prompt() -> Value {
    json!({
        "name": PROMPT,
        "title": "Review comments against code changes",
        "description": "The comment review skill matching this rotter: how to read an extract \
            report and report contradictions between comments and code, and unnecessary comments",
    })
}

fn get_prompt(params: &Map<String, Value>, skill: &str) -> Result<Value, RpcError> {
    match params.get("name").and_then(Value::as_str) {
        Some(PROMPT) => Ok(json!({
            "description": prompt()["description"],
            "messages": [{
                "role": "user",
                "content": { "type": "text", "text": format!("{MCP_GUIDE}\n\n{skill}") },
            }],
        })),
        Some(name) => Err(RpcError::params(format!("unknown prompt: {name}"))),
        None => Err(RpcError::params("prompts/get needs a prompt name")),
    }
}

fn call_tool(
    params: &Map<String, Value>,
    extract: Extract,
    cancel: Arc<AtomicBool>,
) -> Result<Value, RpcError> {
    match params.get("name").and_then(Value::as_str) {
        Some("extract") => {}
        Some(name) => return Err(RpcError::params(format!("unknown tool: {name}"))),
        None => return Err(RpcError::params("tools/call needs a tool name")),
    }
    let empty = Map::new();
    let arguments = match params.get("arguments") {
        None | Some(Value::Null) => &empty,
        Some(Value::Object(arguments)) => arguments,
        Some(_) => return Err(RpcError::params("arguments must be an object")),
    };
    // Bad argument values are the model's to fix: they come back as a tool error, not a
    // protocol one.
    let args = match cli_args(arguments) {
        Ok(args) => args,
        Err(why) => return Ok(tool_error(&why)),
    };
    Ok(match extract(args, cancel) {
        Ok((text, notes)) => match serde_json::from_str::<Value>(&text) {
            Ok(report) => {
                let content: Vec<Value> = std::iter::once(text)
                    .chain(notes)
                    .map(|text| json!({ "type": "text", "text": text }))
                    .collect();
                json!({ "content": content, "structuredContent": report })
            }
            Err(error) => tool_error(&format!("the report is not JSON: {error}")),
        },
        Err(why) => tool_error(&why),
    })
}

fn tool_error(message: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": message }], "isError": true })
}

const ARGUMENTS: [&str; 6] = [
    "mode",
    "base",
    "directory",
    "include_untracked",
    "paths",
    "lang",
];

fn cli_args(arguments: &Map<String, Value>) -> Result<Vec<String>, String> {
    if let Some(unknown) = arguments
        .keys()
        .find(|key| !ARGUMENTS.contains(&key.as_str()))
    {
        return Err(format!("unknown argument: {unknown}"));
    }
    let base = string_arg(arguments, "base")?;
    let mut args = vec!["extract".to_owned()];
    args.extend(mode_args(string_arg(arguments, "mode")?.as_deref(), base)?);
    if bool_arg(arguments, "include_untracked")? {
        args.push("--include-untracked".into());
    }
    for lang in strings_arg(arguments, "lang")? {
        args.extend(["--lang".to_owned(), lang]);
    }
    if let Some(directory) = string_arg(arguments, "directory")? {
        args.extend(["-C".to_owned(), directory]);
    }
    let paths = strings_arg(arguments, "paths")?;
    if !paths.is_empty() {
        args.push("--".into());
        args.extend(paths);
    }
    Ok(args)
}

fn mode_args(mode: Option<&str>, base: Option<String>) -> Result<Vec<String>, String> {
    let flag = match mode {
        Some("staged") => "--staged",
        Some("worktree") => "--worktree",
        Some("full") => "--full",
        Some("base") => {
            let base = base.ok_or("mode base needs base, the revision to compare against")?;
            return Ok(vec!["--base".into(), base]);
        }
        Some(other) => return Err(format!("unknown mode: {other}")),
        None => return Err("mode is required: staged, worktree, base or full".into()),
    };
    match base {
        Some(_) => Err("base applies only to mode base".into()),
        None => Ok(vec![flag.into()]),
    }
}

fn string_arg(arguments: &Map<String, Value>, key: &str) -> Result<Option<String>, String> {
    match arguments.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(format!("{key} must be a string")),
    }
}

fn strings_arg(arguments: &Map<String, Value>, key: &str) -> Result<Vec<String>, String> {
    let wrong = || format!("{key} must be an array of strings");
    match arguments.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().map(str::to_owned).ok_or_else(wrong))
            .collect(),
        Some(_) => Err(wrong()),
    }
}

fn bool_arg(arguments: &Map<String, Value>, key: &str) -> Result<bool, String> {
    match arguments.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(format!("{key} must be a boolean")),
    }
}

#[cfg(test)]
mod tests {
    use super::{MCP_GUIDE, PROTOCOL_VERSION, cli_args, serve};
    use serde_json::{Value, json};
    use std::cell::RefCell;
    use std::io::{BufReader, Cursor, Write};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    /// An extract that ignores the cancel flag; most tests need no more.
    type Extract<'a> = dyn Fn(Vec<String>) -> Result<(String, Vec<String>), String> + 'a;

    fn meta() -> Value {
        json!({
            "io.modelcontextprotocol/protocolVersion": PROTOCOL_VERSION,
            "io.modelcontextprotocol/clientCapabilities": {},
        })
    }

    fn request(id: i64, method: &str, mut params: Value) -> Value {
        params["_meta"] = meta();
        json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
    }

    fn serve_bytes(input: &[u8], extract: &Extract<'_>) -> Vec<Value> {
        let mut output = Vec::new();
        let ignoring = |args, _| extract(args);
        serve(Cursor::new(input.to_vec()), &mut output, &ignoring, "SKILL").unwrap();
        let text = String::from_utf8(output).unwrap();
        text.lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn run(lines: &[Value], extract: &Extract<'_>) -> Vec<Value> {
        let input: String = lines.iter().map(|line| format!("{line}\n")).collect();
        serve_bytes(input.as_bytes(), extract)
    }

    fn report(_: Vec<String>) -> Result<(String, Vec<String>), String> {
        Ok((
            r#"{"tool":"rotter","complete":true}"#.to_owned(),
            Vec::new(),
        ))
    }

    fn slow(_: Vec<String>) -> Result<(String, Vec<String>), String> {
        std::thread::sleep(std::time::Duration::from_millis(200));
        report(Vec::new())
    }

    fn call(id: i64) -> Value {
        request(
            id,
            "tools/call",
            json!({ "name": "extract", "arguments": { "mode": "full" } }),
        )
    }

    fn cancel(id: Value) -> Value {
        json!({ "jsonrpc": "2.0", "method": "notifications/cancelled",
            "params": { "requestId": id } })
    }

    #[test]
    fn a_cancellation_of_the_running_request_sets_its_flag_at_once() {
        let saw = AtomicBool::new(false);
        let (started, has_started) = std::sync::mpsc::channel();
        let waiting = |_: Vec<String>, cancel: Arc<AtomicBool>| {
            started.send(()).unwrap();
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(5) {
                if cancel.load(Ordering::Relaxed) {
                    saw.store(true, Ordering::Relaxed);
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            report(Vec::new())
        };
        // The cancellation is written only once the extract runs, so it cancels a running
        // request, not a waiting one.
        let (reader, mut writer) = std::io::pipe().unwrap();
        let client = std::thread::spawn(move || {
            writeln!(writer, "{}", call(1)).unwrap();
            has_started
                .recv_timeout(Duration::from_secs(10))
                .expect("the extract never started");
            writeln!(writer, "{}", cancel(json!(1))).unwrap();
            writeln!(writer, "{}", request(2, "tools/list", json!({}))).unwrap();
        });
        let mut output = Vec::new();
        let start = Instant::now();
        serve(BufReader::new(reader), &mut output, &waiting, "SKILL").unwrap();
        client.join().unwrap();
        assert!(
            saw.load(Ordering::Relaxed),
            "the extract saw the cancellation"
        );
        assert!(start.elapsed() < Duration::from_secs(4), "it stopped early");
        let text = String::from_utf8(output).unwrap();
        let ids: Vec<Value> = text
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap()["id"].clone())
            .collect();
        assert_eq!(ids, [json!(2)]);
    }

    #[test]
    fn discover_lists_and_prompt_carry_the_required_fields() {
        let responses = run(
            &[
                request(1, "server/discover", json!({})),
                request(2, "tools/list", json!({})),
                request(3, "prompts/list", json!({})),
                request(4, "prompts/get", json!({ "name": "rotter-comment-review" })),
            ],
            &report,
        );
        assert_eq!(responses.len(), 4);
        for (index, response) in responses.iter().enumerate() {
            let result = &response["result"];
            assert_eq!(response["id"], index as i64 + 1);
            assert_eq!(result["resultType"], "complete", "{response}");
            assert_eq!(
                result["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
                "rotter"
            );
        }
        let discover = &responses[0]["result"];
        assert_eq!(discover["supportedVersions"], json!([PROTOCOL_VERSION]));
        assert_eq!(
            discover["capabilities"],
            json!({ "tools": {}, "prompts": {} })
        );
        for cacheable in [&responses[0], &responses[1], &responses[2]] {
            assert!(
                cacheable["result"]["ttlMs"].as_u64().is_some(),
                "{cacheable}"
            );
            assert_eq!(cacheable["result"]["cacheScope"], "public");
        }
        let tool = &responses[1]["result"]["tools"][0];
        assert_eq!(tool["name"], "extract");
        assert_eq!(tool["inputSchema"]["type"], "object");
        assert_eq!(tool["annotations"]["readOnlyHint"], true);
        assert_eq!(
            responses[2]["result"]["prompts"][0]["name"],
            "rotter-comment-review"
        );
        assert_eq!(
            responses[3]["result"]["messages"],
            json!([{ "role": "user",
                "content": { "type": "text", "text": format!("{MCP_GUIDE}\n\nSKILL") } }])
        );
    }

    #[test]
    fn versions_meta_and_legacy_initialize_are_refused() {
        let mut old = request(1, "tools/list", json!({}));
        old["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"] = "2025-11-25".into();
        let mut bare = request(2, "tools/list", json!({}));
        bare["params"].as_object_mut().unwrap().remove("_meta");
        let mut no_capabilities = request(3, "tools/list", json!({}));
        no_capabilities["params"]["_meta"]
            .as_object_mut()
            .unwrap()
            .remove("io.modelcontextprotocol/clientCapabilities");
        let initialize = json!({ "jsonrpc": "2.0", "id": 4, "method": "initialize",
            "params": { "protocolVersion": "2025-11-25", "capabilities": {} } });
        let responses = run(
            &[
                old,
                bare,
                no_capabilities,
                initialize,
                request(5, "ping", json!({})),
            ],
            &report,
        );
        assert_eq!(
            responses[0]["error"],
            json!({ "code": -32022, "message": "Unsupported protocol version",
                "data": { "supported": [PROTOCOL_VERSION], "requested": "2025-11-25" } })
        );
        assert_eq!(responses[1]["error"]["code"], -32602);
        assert_eq!(responses[2]["error"]["code"], -32602);
        assert_eq!(responses[3]["error"]["code"], -32601);
        assert!(
            responses[3]["error"]["message"]
                .as_str()
                .unwrap()
                .contains(PROTOCOL_VERSION),
            "a legacy client learns which version to use"
        );
        assert_eq!(
            responses[3]["error"]["data"]["supported"],
            json!([PROTOCOL_VERSION])
        );
        assert_eq!(responses[4]["error"]["code"], -32601, "ping was removed");
    }

    #[test]
    fn requests_without_jsonrpc_get_errors_and_responses_without_an_id_get_nothing() {
        let input = b"{\"id\":4,\"method\":\"tools/list\"}\n{\"id\":5}\n\
            {\"jsonrpc\":\"2.0\",\"error\":{\"code\":-32600,\"message\":\"x\"}}\n";
        let responses = serve_bytes(input, &report);
        assert_eq!(responses.len(), 2, "{responses:?}");
        for (response, id) in responses.iter().zip([4, 5]) {
            assert_eq!(response["id"], id);
            assert_eq!(response["error"]["code"], -32600);
        }
        let mut unknown = request(6, "resources/list", json!({}));
        unknown["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"] = "2027-01-01".into();
        let responses = run(&[unknown], &report);
        assert_eq!(
            responses[0]["error"]["code"], -32022,
            "the version is checked before the method"
        );
    }

    #[test]
    fn malformed_messages_get_errors_without_an_id_and_notifications_get_nothing() {
        let mut input = b"not json\n\xff\xfe\n[1]\n{}\n".to_vec();
        input.extend_from_slice(
            b"{\"jsonrpc\":\"2.0\",\"id\":null,\"method\":\"tools/list\"}\n\
            {\"jsonrpc\":\"2.0\",\"method\":1}\n\
            {\"jsonrpc\":\"2.0\",\"method\":\"notifications/whatever\"}\n\
            {\"jsonrpc\":\"2.0\",\"id\":9,\"result\":{}}\n\
            {\"jsonrpc\":\"2.0\",\"id\":8,\"error\":{\"code\":1,\"message\":\"x\"}}\n \r\n",
        );
        input.extend_from_slice(format!("{}\r\n", request(7, "tools/list", json!({}))).as_bytes());
        let responses = serve_bytes(&input, &report);
        let codes: Vec<_> = responses
            .iter()
            .map(|r| r["error"]["code"].clone())
            .collect();
        let parse = json!(-32700);
        let invalid = json!(-32600);
        assert_eq!(
            codes[..6],
            [
                parse.clone(),
                parse,
                invalid.clone(),
                invalid.clone(),
                invalid.clone(),
                invalid
            ],
            "{responses:?}"
        );
        assert!(responses[..6].iter().all(|r| r.get("id").is_none()));
        assert_eq!(
            responses.len(),
            7,
            "a line that is not UTF-8 does not end the stream"
        );
        assert_eq!(responses[6]["id"], 7);
        assert!(responses[6].get("result").is_some());
    }

    #[test]
    fn extract_runs_the_cli_arguments_and_reports_tool_errors_in_the_result() {
        let seen = RefCell::new(Vec::new());
        let extract = |args: Vec<String>| {
            seen.borrow_mut().push(args);
            report(Vec::new())
        };
        let call = |id, arguments: Value| {
            request(
                id,
                "tools/call",
                json!({ "name": "extract", "arguments": arguments }),
            )
        };
        let responses = run(
            &[
                call(
                    1,
                    json!({ "mode": "worktree", "include_untracked": true,
                    "directory": "/repo", "lang": ["*.x=lua"], "paths": ["src", "-weird"] }),
                ),
                call(2, json!({ "mode": "base" })),
                call(3, json!({ "mode": "staged", "colour": true })),
                request(
                    4,
                    "tools/call",
                    json!({ "name": "delete", "arguments": {} }),
                ),
                request(
                    5,
                    "tools/call",
                    json!({ "name": "extract", "arguments": 5 }),
                ),
            ],
            &extract,
        );
        assert_eq!(
            seen.borrow()[0],
            [
                "extract",
                "--worktree",
                "--include-untracked",
                "--lang",
                "*.x=lua",
                "-C",
                "/repo",
                "--",
                "src",
                "-weird"
            ]
        );
        assert_eq!(
            seen.borrow().len(),
            1,
            "invalid arguments never reach extract"
        );
        let ok = &responses[0]["result"];
        assert_eq!(
            ok["structuredContent"],
            json!({ "tool": "rotter", "complete": true })
        );
        assert_eq!(ok["content"][0]["type"], "text");
        assert!(ok.get("isError").is_none());
        for response in &responses[1..3] {
            assert_eq!(response["result"]["isError"], true, "{response}");
            assert_eq!(response["result"]["resultType"], "complete");
        }
        assert_eq!(
            responses[3]["error"]["code"], -32602,
            "an unknown tool is a protocol error"
        );
        assert_eq!(
            responses[4]["error"]["code"], -32602,
            "arguments that are not an object break the CallToolRequest schema"
        );
    }

    #[test]
    fn notes_follow_the_report_as_text_content() {
        let noted = |_: Vec<String>| {
            Ok((
                r#"{"complete":true}"#.to_owned(),
                vec!["config ignored".to_owned()],
            ))
        };
        let responses = run(&[call(1)], &noted);
        assert_eq!(
            responses[0]["result"]["content"],
            json!([{ "type": "text", "text": r#"{"complete":true}"# },
                { "type": "text", "text": "config ignored" }])
        );
    }

    #[test]
    fn a_cancelled_request_gets_no_response() {
        let responses = run(
            &[
                call(1),
                cancel(json!(1)),
                request(2, "tools/list", json!({})),
            ],
            &slow,
        );
        assert_eq!(responses.len(), 1, "{responses:?}");
        assert_eq!(responses[0]["id"], 2);
    }

    #[test]
    fn a_cancellation_drops_only_the_request_with_that_exact_id() {
        let responses = run(
            &[call(1), cancel(json!("1")), call(2), cancel(json!(3))],
            &slow,
        );
        let ids: Vec<_> = responses.iter().map(|r| r["id"].clone()).collect();
        assert_eq!(ids, [json!(1), json!(2)]);
    }

    #[test]
    fn only_a_cancellation_notification_cancels() {
        // Another notification naming the request, and a request with the cancellation's method,
        // cancel nothing.
        let mut as_request = cancel(json!(1));
        as_request["id"] = json!(9);
        let responses = run(
            &[
                call(1),
                json!({ "jsonrpc": "2.0", "method": "notifications/progress",
                    "params": { "requestId": 1 } }),
                as_request,
            ],
            &slow,
        );
        let ids: Vec<_> = responses.iter().map(|r| r["id"].clone()).collect();
        assert_eq!(ids, [json!(1), json!(9)], "{responses:?}");
    }

    #[test]
    fn a_cancellation_of_an_unknown_request_is_ignored() {
        let idle = run(
            &[cancel(json!(1)), request(1, "tools/list", json!({}))],
            &report,
        );
        assert_eq!(idle.len(), 1, "{idle:?}");
        let busy = run(
            &[
                call(1),
                cancel(json!(5)),
                request(5, "tools/list", json!({})),
            ],
            &slow,
        );
        let ids: Vec<_> = busy.iter().map(|r| r["id"].clone()).collect();
        assert_eq!(ids, [json!(1), json!(5)]);
    }

    #[test]
    fn skipping_a_cancelled_request_keeps_the_entry_of_a_new_one_with_its_id() {
        // The first 5 is cancelled while it waits and skipped; the second 5 then runs and is
        // cancelled only after it started, which must still reach its flag.
        let runs = std::cell::Cell::new(0);
        let (started, has_started) = std::sync::mpsc::channel();
        let extract = |_: Vec<String>, cancel: Arc<AtomicBool>| {
            runs.set(runs.get() + 1);
            if runs.get() == 1 {
                std::thread::sleep(Duration::from_millis(200));
                return report(Vec::new());
            }
            started.send(()).unwrap();
            let start = Instant::now();
            while !cancel.load(Ordering::Relaxed) && start.elapsed() < Duration::from_secs(5) {
                std::thread::sleep(Duration::from_millis(5));
            }
            report(Vec::new())
        };
        let (reader, mut writer) = std::io::pipe().unwrap();
        let client = std::thread::spawn(move || {
            for line in [call(1), call(5), cancel(json!(5)), call(5)] {
                writeln!(writer, "{line}").unwrap();
            }
            has_started
                .recv_timeout(Duration::from_secs(10))
                .expect("the extract never started");
            writeln!(writer, "{}", cancel(json!(5))).unwrap();
            writeln!(writer, "{}", request(6, "tools/list", json!({}))).unwrap();
        });
        let mut output = Vec::new();
        serve(BufReader::new(reader), &mut output, &extract, "SKILL").unwrap();
        client.join().unwrap();
        let ids: Vec<Value> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap()["id"].clone())
            .collect();
        assert_eq!(ids, [json!(1), json!(6)]);
        assert_eq!(runs.get(), 2, "the first 5 never ran");
    }

    #[test]
    fn an_id_reused_after_a_cancellation_names_only_the_new_request() {
        let responses = run(
            &[
                call(1),
                request(5, "prompts/list", json!({})),
                cancel(json!(5)),
                request(5, "tools/list", json!({})),
                cancel(json!(5)),
                request(6, "tools/list", json!({})),
            ],
            &slow,
        );
        let ids: Vec<_> = responses.iter().map(|r| r["id"].clone()).collect();
        assert_eq!(ids, [json!(1), json!(6)], "{responses:?}");
    }

    #[test]
    fn a_request_cancelled_while_queued_never_runs() {
        let runs = RefCell::new(0);
        let counted = |args: Vec<String>| {
            *runs.borrow_mut() += 1;
            slow(args)
        };
        let responses = run(
            &[
                call(1),
                call(2),
                cancel(json!(2)),
                request(3, "tools/list", json!({})),
            ],
            &counted,
        );
        let ids: Vec<_> = responses.iter().map(|r| r["id"].clone()).collect();
        assert_eq!(ids, [json!(1), json!(3)]);
        assert_eq!(*runs.borrow(), 1, "the cancelled extract never ran");
    }

    #[test]
    fn string_and_negative_ids_are_answered_with_the_same_id() {
        let mut named = request(0, "tools/list", json!({}));
        named["id"] = "a".into();
        let responses = run(&[named, request(-1, "tools/list", json!({}))], &report);
        assert_eq!(responses[0]["id"], "a");
        assert_eq!(responses[1]["id"], -1);
        assert!(
            responses.iter().all(|r| r.get("result").is_some()),
            "{responses:?}"
        );
    }

    #[test]
    fn a_cursor_is_refused_because_every_list_has_one_page() {
        let responses = run(
            &[
                request(1, "tools/list", json!({ "cursor": "next" })),
                request(2, "prompts/list", json!({ "cursor": "next" })),
                request(3, "tools/list", json!({ "cursor": null })),
            ],
            &report,
        );
        assert_eq!(responses[0]["error"]["code"], -32602);
        assert_eq!(responses[1]["error"]["code"], -32602);
        assert!(responses[2].get("result").is_some(), "{}", responses[2]);
    }

    #[test]
    fn cli_args_map_every_mode_and_refuse_mismatches() {
        let args = |value: Value| cli_args(value.as_object().unwrap());
        assert_eq!(
            args(json!({ "mode": "staged" })).unwrap(),
            ["extract", "--staged"]
        );
        assert_eq!(
            args(json!({ "mode": "full" })).unwrap(),
            ["extract", "--full"]
        );
        assert_eq!(
            args(json!({ "mode": "base", "base": "main" })).unwrap(),
            ["extract", "--base", "main"]
        );
        assert!(args(json!({ "mode": "worktree", "base": "main" })).is_err());
        assert!(args(json!({ "mode": "sideways" })).is_err());
        assert!(args(json!({})).is_err());
        assert!(args(json!({ "mode": "full", "paths": "src" })).is_err());
        assert!(args(json!({ "mode": "full", "include_untracked": "yes" })).is_err());
    }
}
