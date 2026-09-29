//! The pi, letta and opencode shims: install, ownership and status through the CLI (HOME, XDG_*
//! and every host directory variable pinned to temp), and each rendered shim run under every
//! engine its host uses (Node and Bun, pinned in mise.toml) against a stub host API and a stub
//! rotter. No case runs `rotter hook pi|letta|opencode` on a real stop: those hosts take HOME
//! from the password database.

mod common;

use common::{document, result, rotter, set_mode, status_host, stdout, temp};
use rotter::integration::render_shim;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const EXE: &str = env!("CARGO_BIN_EXE_rotter");
const HOSTS: [(&str, &str); 3] = [
    ("pi", "rotter-review.ts"),
    ("letta", "rotter-review.js"),
    ("opencode", "rotter-review.js"),
];

/// The engines each host runs its shims under (docs/hosts.md): Pi's npm package on Node and its
/// release binary on Bun, Letta's npm package on Node, OpenCode's binary on Bun.
fn engines(host: &str) -> &'static [&'static str] {
    match host {
        "pi" => &["node", "bun"],
        "letta" => &["node"],
        _ => &["bun"],
    }
}

fn integration(root: &Path, args: &[&str]) -> (i32, String) {
    let mut all = vec!["integration"];
    all.extend(args);
    let output = rotter(root, &all, root, "", &[]);
    let text = stdout(&output) + &String::from_utf8_lossy(&output.stderr);
    (output.status.code().unwrap(), text)
}

fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
}

/// The host dir `rotter` (see common) points each host at, and its shim.
fn paths(root: &Path, host: &str) -> (PathBuf, PathBuf) {
    match host {
        "pi" => (root.join("pi"), root.join("pi/extensions/rotter-review.ts")),
        "opencode" => (
            root.join("opencode"),
            root.join("opencode/plugins/rotter-review.js"),
        ),
        _ => (
            root.join("home/.letta"),
            root.join("home/.letta/mods/rotter-review.js"),
        ),
    }
}

/// The host's `(state, detail)` in `status`.
fn status_of(root: &Path, host: &str) -> (String, String) {
    let (_, text) = integration(root, &["status"]);
    let found = status_host(&text, host);
    assert_eq!(found["support"], "experimental", "{text}");
    (
        found["state"].as_str().unwrap().to_owned(),
        found["detail"].as_str().unwrap().to_owned(),
    )
}

#[test]
fn shims_install_exactly_their_render_and_leave_foreign_code_alone() {
    for (host, _) in HOSTS {
        let root = temp("shim-install");
        let (dir, shim) = paths(&root, host);
        let (code, text) = integration(&root, &["install", host]);
        assert_eq!((code, text.contains("does not exist")), (2, true), "{text}");
        assert!(!dir.exists());
        fs::create_dir_all(&dir).unwrap();
        set_mode(&dir, 0o700);
        let (code, text) = integration(&root, &["install", host]);
        assert_eq!(code, 0, "{text}");
        let done = document(&text, "rotter.integration/1");
        assert_eq!(done["result"], "installed", "{text}");
        assert!(
            done["path"]
                .as_str()
                .unwrap()
                .ends_with(&shim.display().to_string()),
            "{text}"
        );
        assert!(
            done["notes"][0]
                .as_str()
                .unwrap()
                .ends_with("loads it at its next start"),
            "{text}"
        );
        // The default parse timeout (60) gives 90 seconds.
        let rendered = render_shim(host, EXE, 90).unwrap();
        assert_eq!(fs::read_to_string(&shim).unwrap(), rendered);
        assert_eq!(mode(&shim), 0o600);
        assert_eq!(mode(shim.parent().unwrap()), 0o700);
        let (code, text) = integration(&root, &["install", host]);
        assert_eq!(
            (code, result(&text).as_str()),
            (0, "already_installed"),
            "{text}"
        );
        let (state, detail) = status_of(&root, host);
        assert_eq!(
            (state.as_str(), detail.as_str()),
            ("installed", "installed (current)")
        );
        // Another rotter binary's shim: named by status, replaced by install.
        fs::write(&shim, render_shim(host, "/else/rotter", 90).unwrap()).unwrap();
        let (state, detail) = status_of(&root, host);
        assert_eq!(state, "other_binary");
        assert_eq!(detail, "installed for another binary: /else/rotter");
        let (code, text) = integration(&root, &["install", host]);
        assert_eq!((code, result(&text).as_str()), (0, "updated"), "{text}");
        assert_eq!(fs::read_to_string(&shim).unwrap(), rendered);
        // A different timeout is a mismatch.
        fs::write(&shim, render_shim(host, EXE, 61).unwrap()).unwrap();
        let (state, detail) = status_of(&root, host);
        assert_eq!(state, "mismatch");
        assert!(detail.contains("timeout 61, expected 90"), "{detail}");
        // Foreign code at rotter's path: never rewritten or removed.
        let foreign = rendered.replace("MAX_REQUESTS = 2", "MAX_REQUESTS = 9");
        fs::write(&shim, &foreign).unwrap();
        for args in [&["install", host][..], &["uninstall", host]] {
            let (code, text) = integration(&root, args);
            assert_eq!(code, 2, "{args:?}: {text}");
            assert!(text.contains("foreign code at rotter's path"), "{text}");
            assert_eq!(fs::read_to_string(&shim).unwrap(), foreign);
        }
        let (state, detail) = status_of(&root, host);
        assert_eq!(state, "foreign");
        assert!(
            detail.starts_with("foreign code at rotter's path, auto-loaded by"),
            "{detail}"
        );
        // Unsafe permissions and a symlink are refused and left as they are.
        fs::write(&shim, &rendered).unwrap();
        set_mode(&shim, 0o622);
        let (code, text) = integration(&root, &["install", host]);
        assert_eq!(
            (code, text.contains("unsafe permissions")),
            (2, true),
            "{text}"
        );
        assert_eq!(mode(&shim), 0o622);
        fs::remove_file(&shim).unwrap();
        let target = root.join("elsewhere.js");
        fs::write(&target, &rendered).unwrap();
        std::os::unix::fs::symlink(&target, &shim).unwrap();
        let (code, _) = integration(&root, &["install", host]);
        assert_eq!(code, 2);
        assert!(
            fs::symlink_metadata(&shim)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        fs::remove_file(&shim).unwrap();
        // No temporary is left where the host loads code.
        let (code, text) = integration(&root, &["install", host]);
        assert_eq!(code, 0, "{text}");
        let names: Vec<_> = fs::read_dir(shim.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
        let (code, text) = integration(&root, &["uninstall", host]);
        assert_eq!((code, result(&text).as_str()), (0, "removed"), "{text}");
        assert!(!shim.exists());
        let (code, text) = integration(&root, &["uninstall", host]);
        assert_eq!(
            (code, result(&text).as_str()),
            (0, "not_installed"),
            "{text}"
        );
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn pi_falls_back_to_home_when_its_variable_is_unset_or_relative() {
    for value in [None, Some("relative/pi")] {
        let root = temp("shim-fallback");
        let dir = root.join("home/.pi/agent");
        fs::create_dir_all(&dir).unwrap();
        let env = [("PI_CODING_AGENT_DIR", value.map(std::ffi::OsStr::new))];
        let output = rotter(&root, &["integration", "install", "pi"], &root, "", &env);
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        assert!(dir.join("extensions/rotter-review.ts").is_file());
        assert!(!root.join("relative").exists());
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn opencode_falls_back_to_the_xdg_config_base() {
    // (OPENCODE_CONFIG_DIR, XDG_CONFIG_HOME) → the directory install writes under.
    let cases = [
        (None, Some("xdg"), "xdg/opencode"),
        (Some("relative/oc"), Some("xdg"), "xdg/opencode"),
        (None, None, "home/.config/opencode"),
        (None, Some("relative/xdg"), "home/.config/opencode"),
    ];
    for (dir, xdg, expected) in cases {
        let root = temp("shim-opencode-fallback");
        let expected = root.join(expected);
        fs::create_dir_all(&expected).unwrap();
        let absolute = |value: &str| {
            if value.starts_with("relative") {
                value.into()
            } else {
                root.join(value).into_os_string()
            }
        };
        let (dir, xdg) = (dir.map(absolute), xdg.map(absolute));
        let env = [
            ("OPENCODE_CONFIG_DIR", dir.as_deref()),
            ("XDG_CONFIG_HOME", xdg.as_deref()),
        ];
        let output = rotter(
            &root,
            &["integration", "install", "opencode"],
            &root,
            "",
            &env,
        );
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        assert!(
            expected.join("plugins/rotter-review.js").is_file(),
            "{expected:?}"
        );
        assert!(!root.join("relative").exists());
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn golden_renders_are_stable() {
    // A hostile but accepted path: quotes, a backslash, spaces and non-ASCII. A change to a
    // shipped template breaks ownership of the files it wrote: add a new version instead.
    let exe = "/opt/it's \"q\"\\ dir/ünï/rotter";
    for (host, file) in HOSTS {
        let golden = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/shim")
            .join(format!(
                "{host}-v1.golden{}",
                &file[file.rfind('.').unwrap()..]
            ));
        assert_eq!(
            render_shim(host, exe, 90).unwrap(),
            fs::read_to_string(&golden).unwrap(),
            "{host}"
        );
    }
}

/// `tool`'s binary at the version mise.toml pins, from mise's install directory, and that
/// version. A missing runtime fails the test: no shim ships without running under its engine.
fn runtime(tool: &str) -> (PathBuf, String) {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let toml = fs::read_to_string(manifest.join("mise.toml")).unwrap();
    let pin = format!("{tool} = \"");
    let version = toml
        .lines()
        .find_map(|line| line.strip_prefix(&pin)?.strip_suffix('"'))
        .unwrap_or_else(|| panic!("mise.toml pins {tool}"));
    let data = std::env::var_os("MISE_DATA_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_DATA_HOME").map(|dir| PathBuf::from(dir).join("mise")))
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share/mise"))
        })
        .expect("MISE_DATA_DIR, XDG_DATA_HOME or HOME");
    let binary = data
        .join("installs")
        .join(tool)
        .join(version)
        .join("bin")
        .join(tool);
    assert!(
        binary.is_file(),
        "{tool} {version} (pinned in mise.toml) is not installed at {}; run `mise install`",
        binary.display()
    );
    (binary, version.to_owned())
}

struct Run {
    engine: String,
    /// OpenCode: how many `session.idle` events the harness emitted.
    idles: u64,
    /// Each stop's answer (the injected review request or null) and how long it took.
    answers: Vec<(Value, u64)>,
    /// The stub rotter's calls: argv, environment, stdin.
    calls: Vec<Value>,
    stub: PathBuf,
}

/// Renders `host`'s shim with a stub rotter (in a directory whose name needs quoting in a shell)
/// and a 1 s timeout, and runs `scenario` of the harness under `engine`. The stub rotter itself
/// always runs on Node.
fn run(host: &str, file: &str, engine: &str, stub_mode: &str, scenario: &str) -> Run {
    let (node, _) = runtime("node");
    let (binary, version) = runtime(engine);
    let root = temp("shim-run");
    let stub_dir = root.join("stub dir 'q' ü");
    fs::create_dir(&stub_dir).unwrap();
    let stub = stub_dir.join("rotter");
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/shim");
    let source = fs::read_to_string(fixtures.join("stub.cjs")).unwrap();
    fs::write(&stub, format!("#!{}\n{source}", node.display())).unwrap();
    set_mode(&stub, 0o755);
    fs::write(stub_dir.join("mode"), stub_mode).unwrap();
    let exe = if stub_mode == "missing" {
        stub_dir.join("absent")
    } else {
        stub.clone()
    };
    let shim = root.join(file);
    fs::write(&shim, render_shim(host, exe.to_str().unwrap(), 1).unwrap()).unwrap();
    let cwd = root.join("work");
    fs::create_dir(&cwd).unwrap();
    let output = Command::new(&binary)
        .arg(fixtures.join("harness.mjs"))
        .args([
            host,
            shim.to_str().unwrap(),
            scenario,
            cwd.to_str().unwrap(),
        ])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LANG", "C.UTF-8")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{host} {engine} {scenario}: {output:?}"
    );
    let report: Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("{host} {engine} {scenario}: {error}: {output:?}"));
    assert_eq!(
        report["fired"],
        serde_json::json!([]),
        "{host} {engine} {scenario}"
    );
    assert_eq!(report["engine"], format!("{engine} {version}"));
    let calls = fs::read_to_string(stub_dir.join("log"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let answers = report["answers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|answer| (answer["answer"].clone(), answer["ms"].as_u64().unwrap()))
        .collect();
    Run {
        engine: report["engine"].as_str().unwrap().to_owned(),
        idles: report["idles"].as_u64().unwrap(),
        answers,
        calls,
        stub: stub_dir,
    }
}

const REQUEST: &str = "review the comments";

/// Every (host, shim file, engine) the runtime cases run.
fn cells() -> Vec<(&'static str, &'static str, &'static str)> {
    HOSTS
        .into_iter()
        .flat_map(|(host, file)| {
            engines(host)
                .iter()
                .map(move |engine| (host, file, *engine))
        })
        .collect()
}

/// Whether process `pid` still exists.
fn alive(pid: &str) -> bool {
    Command::new("/bin/kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
}

#[test]
fn shims_run_rotter_without_a_shell_with_only_path_and_lang() {
    for (host, file, engine) in cells() {
        let run = run(host, file, engine, "reply", "once");
        eprintln!("{host}: shim harness under {}", run.engine);
        assert_eq!(run.answers.len(), 1);
        assert_eq!(run.answers[0].0, REQUEST, "{host} {engine}");
        let [call] = run.calls.as_slice() else {
            panic!("{host} {engine}: {:?}", run.calls);
        };
        // Spawned directly: the exe path with a space and a quote arrived as one argv[0].
        assert_eq!(
            call["argv"],
            serde_json::json!(["hook", host, "--timeout", "1"])
        );
        // macOS CoreFoundation adds its text encoding to the stub's own environment at start.
        let mut env = call["env"].clone();
        env.as_object_mut()
            .unwrap()
            .remove("__CF_USER_TEXT_ENCODING");
        assert_eq!(
            env,
            serde_json::json!({ "PATH": "/usr/bin:/bin", "LANG": "C.UTF-8" }),
            "{host} {engine}: LD_PRELOAD, DYLD_INSERT_LIBRARIES, DEVELOPER_DIR, HOME must not arrive"
        );
        let stdin: Value = serde_json::from_str(call["stdin"].as_str().unwrap()).unwrap();
        let cwd = stdin["cwd"].as_str().unwrap();
        assert_eq!(stdin, serde_json::json!({ "session_id": "s", "cwd": cwd }));
        assert!(cwd.ends_with("/work"), "{cwd}");
        let _ = fs::remove_dir_all(run.stub.parent().unwrap());
    }
}

#[test]
fn shims_cap_consecutive_requests_and_run_one_at_a_time() {
    for (host, file, engine) in cells() {
        let run = run(host, file, engine, "reply", "loop");
        let answers: Vec<&Value> = run.answers.iter().map(|(answer, _)| answer).collect();
        let request = Value::from(REQUEST);
        assert_eq!(
            answers,
            [
                &request,
                &request,
                &Value::Null,
                &request,
                &request,
                &Value::Null
            ],
            "{host} {engine}"
        );
        // The suppressed and the concurrent stop never reached rotter.
        assert_eq!(run.calls.len(), 4, "{host} {engine}");
        let _ = fs::remove_dir_all(run.stub.parent().unwrap());
    }
}

/// OpenCode's re-prompt starts a turn whose idle comes back to the plugin: the stub
/// `promptAsync` emits the new user message and that idle before it returns. The in-flight flag
/// is not held for the injected turn (the second idle asks again), the user message does not
/// reset the cap, and across three changing reports the third request is still suppressed.
#[test]
fn opencode_re_prompts_are_capped_across_the_turns_they_start() {
    for engine in engines("opencode") {
        let chain = run("opencode", "rotter-review.js", engine, "reply", "chain");
        let answers: Vec<&Value> = chain.answers.iter().map(|(answer, _)| answer).collect();
        assert_eq!(answers, [REQUEST, REQUEST], "{engine}");
        assert_eq!((chain.idles, chain.calls.len()), (3, 2), "{engine}");
        let _ = fs::remove_dir_all(chain.stub.parent().unwrap());
        // A rejected re-prompt is swallowed.
        let rejected = run("opencode", "rotter-review.js", engine, "reply", "rejecting");
        assert_eq!(rejected.answers.len(), 1, "{engine}");
        assert_eq!(rejected.answers[0].0, REQUEST, "{engine}");
        let _ = fs::remove_dir_all(rejected.stub.parent().unwrap());
    }
}

#[test]
fn shims_resolve_quietly_whatever_rotter_does() {
    // (stub mode, scenario, fastest, slowest) in ms; every answer is null, so OpenCode is never
    // re-prompted.
    let cases = [
        ("quiet", "once", 0, 900),
        ("nonstring", "once", 0, 900),
        ("garbage", "once", 0, 900),
        ("epipe", "once", 0, 900),
        ("missing", "once", 0, 900),
        ("big", "once", 0, 900),
        ("bigexit", "once", 0, 900),
        ("hang", "once", 900, 3000),
        ("reply", "skipped", 0, 100),
        ("reply", "throwing", 0, 100),
    ];
    for (host, file, engine) in cells() {
        for (stub_mode, scenario, fastest, slowest) in cases {
            let run = run(host, file, engine, stub_mode, scenario);
            let [(answer, ms)] = run.answers.as_slice() else {
                panic!("{host} {engine} {stub_mode}: {:?}", run.answers);
            };
            assert_eq!(
                answer,
                &Value::Null,
                "{host} {engine} {stub_mode} {scenario}"
            );
            assert!(
                (fastest..=slowest).contains(ms),
                "{host} {engine} {stub_mode} {scenario}: {ms} ms"
            );
            if scenario != "once" || stub_mode == "missing" {
                assert!(
                    run.calls.is_empty(),
                    "{host} {engine} {stub_mode} {scenario}"
                );
            }
            if stub_mode == "hang" {
                // The timeout killed rotter's whole process group: the grandchild would sleep
                // well past the harness, which exits without waiting for it.
                let pid = fs::read_to_string(run.stub.join("grandchild")).unwrap();
                let survived = alive(pid.trim());
                if survived {
                    let _ = Command::new("/bin/kill").args(["-9", pid.trim()]).status();
                }
                assert!(!survived, "{host} {engine}: the grandchild {pid} survived");
            }
            let _ = fs::remove_dir_all(run.stub.parent().unwrap());
        }
        // Answering exactly as the timeout fires settles once, either way.
        let run = run(host, file, engine, "attimeout", "once");
        assert_eq!(run.answers.len(), 1, "{host} {engine}");
        let _ = fs::remove_dir_all(run.stub.parent().unwrap());
    }
}
