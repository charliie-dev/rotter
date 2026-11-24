//! `rotter mcp` end to end: the binary on a real repository, over stdin and stdout.

mod common;

use serde_json::{Value, json};
use std::fs;

fn request(id: i64, method: &str, params: Value) -> String {
    let mut params = params;
    params["_meta"] = json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {},
        "io.modelcontextprotocol/clientInfo": { "name": "test", "version": "0" },
    });
    format!(
        "{}\n",
        json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
    )
}

#[test]
fn mcp_serves_extract_and_the_skill_until_stdin_closes() {
    let root = common::temp("mcp");
    let repo = root.join("repo");
    common::changed_repo(&repo);
    let input = [
        request(1, "server/discover", json!({})),
        request(
            2,
            "tools/call",
            json!({ "name": "extract", "arguments": {
                "mode": "worktree", "directory": repo.display().to_string() } }),
        ),
        request(
            3,
            "tools/call",
            json!({ "name": "extract", "arguments": {
                "mode": "worktree", "directory": root.join("nowhere").display().to_string() } }),
        ),
        request(4, "prompts/get", json!({ "name": "rotter-comment-review" })),
    ]
    .concat();
    let output = common::rotter(&root, &["mcp"], &root, &input, &[]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    // stdout carries only the four responses, one JSON object per line.
    let responses: Vec<Value> = stdout
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(responses.len(), 4, "{stdout}");

    assert_eq!(
        responses[0]["result"]["supportedVersions"],
        json!(["2026-07-28"])
    );
    assert_eq!(
        responses[0]["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["version"],
        env!("CARGO_PKG_VERSION")
    );

    let call = &responses[1]["result"];
    assert_eq!(call["resultType"], "complete");
    assert!(call.get("isError").is_none(), "{call}");
    let report = &call["structuredContent"];
    assert_eq!(report["schema"], "rotter.extract.poc/0");
    assert_eq!(report["mode"], "worktree");
    assert_eq!(report["files"][0]["new_path"], "a.go");
    assert_eq!(
        serde_json::from_str::<Value>(call["content"][0]["text"].as_str().unwrap()).unwrap(),
        *report,
        "the text content is the same report"
    );

    let failed = &responses[2]["result"];
    assert_eq!(failed["isError"], true, "{failed}");
    assert!(!failed["content"][0]["text"].as_str().unwrap().is_empty());

    assert_eq!(
        responses[3]["result"]["messages"][0]["content"]["text"],
        fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/skills/rotter-comment-review/SKILL.md"
        ))
        .unwrap()
    );
    fs::remove_dir_all(&root).unwrap();
}
