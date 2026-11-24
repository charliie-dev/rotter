//! `rotter mcp`: a Model Context Protocol server on stdio, protocol version 2026-07-28 only.
//!
//! The protocol is stateless: there is no `initialize` handshake, and every request carries its
//! protocol version and client capabilities in `_meta`. The server offers one read-only tool,
//! `extract` (the CLI's `rotter extract`, run through the same code), and one prompt, the comment
//! review skill. Messages are newline-delimited JSON-RPC 2.0 on stdin and stdout; stdout carries
//! nothing else, and the server exits when stdin closes.

use serde_json::{Map, Value, json};
use std::collections::{HashSet, VecDeque};
use std::io::{BufRead, Write};
use std::sync::mpsc;

pub const PROTOCOL_VERSION: &str = "2026-07-28";

/// The tool and prompt lists never change while the binary runs; clients may cache them.
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

/// Runs `rotter extract` with these CLI arguments, `extract` first.
pub type Extract<'a> = &'a dyn Fn(Vec<String>) -> Result<(String, bool), String>;

struct RpcError(i64, String, Option<Value>);

impl RpcError {
    fn params(message: impl Into<String>) -> Self {
        Self(INVALID_PARAMS, message.into(), None)
    }
}

/// Serves requests read from `input` until it ends, writing every message to `output`.
pub fn serve<R, W>(input: R, mut output: W, extract: Extract, skill: &str) -> std::io::Result<()>
where
    R: BufRead + Send + 'static,
    W: Write,
{
    // A reader thread keeps taking messages while a request runs, so a cancellation sent during
    // a long extract is seen before its response would be written.
    let (sender, receiver) = mpsc::channel::<std::io::Result<String>>();
    std::thread::spawn(move || {
        for line in input.lines() {
            if sender.send(line).is_err() {
                break;
            }
        }
    });
    let mut queue: VecDeque<String> = VecDeque::new();
    let mut cancelled: HashSet<String> = HashSet::new();
    loop {
        let line = match queue.pop_front() {
            Some(line) => line,
            None => match receiver.recv() {
                Ok(line) => line?,
                Err(_) => return Ok(()),
            },
        };
        if line.trim().is_empty() {
            continue;
        }
        let Some((id, response)) = handle(&line, extract, skill, &mut cancelled) else {
            continue;
        };
        // Messages that arrived while this request ran: note cancellations, keep the rest.
        while let Ok(next) = receiver.try_recv() {
            let next = next?;
            if !note_cancellation(&next, &mut cancelled) {
                queue.push_back(next);
            }
        }
        if id.as_ref().is_some_and(|id| cancelled.remove(&id_key(id))) {
            continue;
        }
        let mut text = serde_json::to_string(&response).map_err(std::io::Error::other)?;
        text.push('\n');
        output.write_all(text.as_bytes())?;
        output.flush()?;
    }
}

/// The request's `id` as a set key: `1` and `"1"` are different ids.
fn id_key(id: &Value) -> String {
    id.to_string()
}

/// Records a `notifications/cancelled` message; false for anything else.
fn note_cancellation(line: &str, cancelled: &mut HashSet<String>) -> bool {
    let Ok(message) = serde_json::from_str::<Value>(line) else {
        return false;
    };
    if message.get("id").is_some() || message["method"] != "notifications/cancelled" {
        return false;
    }
    if let Some(id) = message["params"].get("requestId") {
        cancelled.insert(id_key(id));
    }
    true
}

/// The response to one message (with the request id it answers), or None for a notification.
fn handle(
    line: &str,
    extract: Extract,
    skill: &str,
    cancelled: &mut HashSet<String>,
) -> Option<(Option<Value>, Value)> {
    let message: Value = match serde_json::from_str(line) {
        Ok(message) => message,
        Err(error) => {
            let error = RpcError(PARSE_ERROR, format!("parse error: {error}"), None);
            return Some((None, error_response(&Value::Null, error)));
        }
    };
    let Some(object) = message.as_object() else {
        let error = RpcError(
            INVALID_REQUEST,
            "a message must be one JSON object".into(),
            None,
        );
        return Some((None, error_response(&Value::Null, error)));
    };
    let Some(id) = object.get("id").cloned() else {
        // A notification: only cancellations mean anything to this server.
        note_cancellation(line, cancelled);
        return None;
    };
    let valid_id = id.is_string() || id.is_i64() || id.is_u64();
    let method = object.get("method").and_then(Value::as_str);
    let result = match (object.get("jsonrpc"), method) {
        (Some(version), Some(method)) if version == "2.0" && valid_id => {
            dispatch(method, object.get("params"), extract, skill)
        }
        _ => Err(RpcError(
            INVALID_REQUEST,
            "expected a JSON-RPC 2.0 request with a string or integer id and a method".into(),
            None,
        )),
    };
    let reply_id = if valid_id { id.clone() } else { Value::Null };
    let response = match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": reply_id, "result": result }),
        Err(error) => error_response(&reply_id, error),
    };
    Some((valid_id.then_some(id), response))
}

fn error_response(id: &Value, RpcError(code, message, data): RpcError) -> Value {
    let mut error = json!({ "code": code, "message": message });
    if let Some(data) = data {
        error["data"] = data;
    }
    json!({ "jsonrpc": "2.0", "id": id, "error": error })
}

fn dispatch(
    method: &str,
    params: Option<&Value>,
    extract: Extract,
    skill: &str,
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
    if !known.contains(&method) {
        return Err(RpcError(
            METHOD_NOT_FOUND,
            format!("method not found: {method}"),
            None,
        ));
    }
    let empty = Map::new();
    let params = match params {
        None => &empty,
        Some(Value::Object(params)) => params,
        Some(_) => return Err(RpcError::params("params must be an object")),
    };
    check_meta(params)?;
    let result = match method {
        "server/discover" => discover(),
        "tools/list" => {
            no_cursor(params)?;
            list(json!({ "tools": [extract_tool()] }))
        }
        "tools/call" => call_tool(params, extract)?,
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

/// Adds the fields every result carries.
fn complete(mut result: Value) -> Value {
    result["resultType"] = "complete".into();
    result["_meta"] = json!({
        SERVER_INFO_KEY: { "name": "rotter", "version": env!("CARGO_PKG_VERSION") }
    });
    result
}

/// A list result clients may cache: nothing in it depends on the user.
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
            writes the repository or its index and never runs repository hooks or filters. \
            `complete` is false when some changed file could not be analysed.",
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
                    "description": "A directory inside the repository (default: the server's \
                        working directory)",
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
            "messages": [{ "role": "user", "content": { "type": "text", "text": skill } }],
        })),
        Some(name) => Err(RpcError::params(format!("unknown prompt: {name}"))),
        None => Err(RpcError::params("prompts/get needs a prompt name")),
    }
}

fn call_tool(params: &Map<String, Value>, extract: Extract) -> Result<Value, RpcError> {
    match params.get("name").and_then(Value::as_str) {
        Some("extract") => {}
        Some(name) => return Err(RpcError::params(format!("unknown tool: {name}"))),
        None => return Err(RpcError::params("tools/call needs a tool name")),
    }
    let empty = Map::new();
    let arguments = match params.get("arguments") {
        None | Some(Value::Null) => &empty,
        Some(Value::Object(arguments)) => arguments,
        Some(_) => return Ok(tool_error("arguments must be an object")),
    };
    // Bad arguments are the model's to fix: they come back as a tool error, not a protocol one.
    let args = match cli_args(arguments) {
        Ok(args) => args,
        Err(why) => return Ok(tool_error(&why)),
    };
    Ok(match extract(args) {
        Ok((text, _)) => match serde_json::from_str::<Value>(&text) {
            Ok(report) => json!({
                "content": [{ "type": "text", "text": text }],
                "structuredContent": report,
            }),
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
    use super::{PROTOCOL_VERSION, cli_args, serve};
    use serde_json::{Value, json};
    use std::cell::RefCell;
    use std::io::Cursor;

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

    /// Serves `lines` with an extract that records its arguments; returns the responses.
    fn run(
        lines: &[Value],
        extract: &dyn Fn(Vec<String>) -> Result<(String, bool), String>,
    ) -> Vec<Value> {
        let input: String = lines.iter().map(|line| format!("{line}\n")).collect();
        let mut output = Vec::new();
        serve(
            Cursor::new(input.into_bytes()),
            &mut output,
            extract,
            "SKILL",
        )
        .unwrap();
        let text = String::from_utf8(output).unwrap();
        text.lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn report(_: Vec<String>) -> Result<(String, bool), String> {
        Ok((r#"{"tool":"rotter","complete":true}"#.to_owned(), true))
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
            json!([{ "role": "user", "content": { "type": "text", "text": "SKILL" } }])
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
    fn malformed_messages_get_errors_and_notifications_get_nothing() {
        let input = "not json\n[1]\n{\"jsonrpc\":\"2.0\",\"id\":null,\"method\":\"tools/list\"}\n\
            {\"jsonrpc\":\"2.0\",\"method\":\"notifications/whatever\"}\n\n";
        let mut output = Vec::new();
        serve(
            Cursor::new(input.as_bytes().to_vec()),
            &mut output,
            &report,
            "",
        )
        .unwrap();
        let responses: Vec<Value> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let codes: Vec<_> = responses
            .iter()
            .map(|r| r["error"]["code"].clone())
            .collect();
        assert_eq!(codes, [json!(-32700), json!(-32600), json!(-32600)]);
        assert!(responses.iter().all(|r| r["id"].is_null()));
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
    }

    #[test]
    fn a_cancelled_request_gets_no_response() {
        let slow = |_: Vec<String>| {
            std::thread::sleep(std::time::Duration::from_millis(200));
            report(Vec::new())
        };
        let responses = run(
            &[
                request(
                    1,
                    "tools/call",
                    json!({ "name": "extract", "arguments": { "mode": "full" } }),
                ),
                json!({ "jsonrpc": "2.0", "method": "notifications/cancelled",
                    "params": { "requestId": 1 } }),
                request(2, "tools/list", json!({})),
            ],
            &slow,
        );
        assert_eq!(responses.len(), 1, "{responses:?}");
        assert_eq!(responses[0]["id"], 2);
    }

    #[test]
    fn a_cancellation_drops_only_the_request_with_that_exact_id() {
        let slow = |_: Vec<String>| {
            std::thread::sleep(std::time::Duration::from_millis(200));
            report(Vec::new())
        };
        let call = |id| {
            request(
                id,
                "tools/call",
                json!({ "name": "extract", "arguments": { "mode": "full" } }),
            )
        };
        let cancel = |id: Value| {
            json!({ "jsonrpc": "2.0", "method": "notifications/cancelled",
                "params": { "requestId": id } })
        };
        let responses = run(
            &[call(1), cancel(json!("1")), call(2), cancel(json!(3))],
            &slow,
        );
        let ids: Vec<_> = responses.iter().map(|r| r["id"].clone()).collect();
        assert_eq!(ids, [json!(1), json!(2)]);
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
