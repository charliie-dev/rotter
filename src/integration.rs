use crate::json::Json;
use crate::{Mode, Options, extract};
use serde_json::{Value, json};
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Marks the settings entry this binary owns; everything else in settings.json is left alone.
const HOOK_ARGS: &str = "hook claude-stop";

fn home() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from)
}

fn state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("ROTTER_STATE_DIR") {
        return PathBuf::from(dir);
    }
    std::env::var_os("XDG_STATE_HOME")
        .map_or_else(|| home().join(".local/state"), PathBuf::from)
        .join("rotter")
}

fn claude_settings() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map_or_else(|| home().join(".claude"), PathBuf::from)
        .join("settings.json")
}

fn exe() -> String {
    std::env::current_exe().map_or_else(|_| "rotter".to_owned(), |path| path.display().to_string())
}

fn units(report: &Json) -> usize {
    report
        .get("files")
        .as_arr()
        .iter()
        .flat_map(|file| [file.get("before"), file.get("after")])
        .map(|side| side.get("units").as_arr().len())
        .sum()
}

/// Claude Code Stop hook: returns the JSON to print, or None to let the turn end quietly.
///
/// Blocks once when the working tree has changes that relate to comments, never blocks a
/// continuation it caused, and asks again only after the report changes.
pub fn claude_stop(input: &str) -> Option<String> {
    let input: Value = serde_json::from_str(input).unwrap_or(Value::Null);
    // Claude Code sends stop_hook_active; Grok Build sends stopHookActive.
    if ["stop_hook_active", "stopHookActive"]
        .iter()
        .any(|key| input[key] == Value::Bool(true))
    {
        return None;
    }
    let cwd = input["cwd"]
        .as_str()
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())?;
    let inside = Command::new("git")
        .arg("-C")
        .arg(&cwd)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .is_ok_and(|output| output.status.success());
    if !inside {
        return None;
    }
    let message =
        |text: String| Some(json!({ "systemMessage": format!("rotter: {text}") }).to_string());
    let rotter = exe();
    let command = format!(
        "{rotter} extract --worktree --include-untracked -C {}",
        cwd.display()
    );
    let options = Options {
        mode: Mode::Worktree,
        include_untracked: true,
        paths: Vec::new(),
        languages: Vec::new(),
    };
    let report = match extract(&cwd, &options) {
        Ok(report) => report,
        Err(error) => return message(format!("extract failed: {error}")),
    };
    let found = units(&report.json);
    if found == 0 {
        return (!report.complete)
            .then(|| {
                message(format!(
                    "some changed files could not be analysed; run {command}"
                ))
            })
            .flatten();
    }

    let text = report.json.to_string();
    let mut hasher = DefaultHasher::new();
    (cwd.display().to_string(), &text).hash(&mut hasher);
    let fingerprint = format!("{:016x}", hasher.finish());
    let session: String = input["session_id"]
        .as_str()
        .or(input["sessionId"].as_str())
        .unwrap_or("unknown")
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || "._-".contains(character) {
                character
            } else {
                '_'
            }
        })
        .collect();
    let dir = state_dir().join("claude-stop");
    let state = dir.join(session);
    if fs::read_to_string(&state).is_ok_and(|seen| seen.trim() == fingerprint) {
        return None;
    }
    // Failing to record state only means the same report may be requested again.
    let _ = fs::create_dir_all(&dir).and_then(|()| fs::write(&state, format!("{fingerprint}\n")));
    let reason = format!(
        "rotter found {found} changed code unit(s) with related comments in {} (report complete: {}). \
         Before finishing, review them with the rotter-comment-review skill in working tree mode \
         including untracked files: run `{command}`. If that skill is not loaded, run \
         `{rotter} --skill` and follow its output. Report only concrete contradictions between \
         comments and code; do not edit files unless the user asked for it.",
        cwd.display(),
        report.complete
    );
    Some(json!({ "decision": "block", "reason": reason }).to_string())
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

fn is_ours(entry: &Value) -> bool {
    entry["command"]
        .as_str()
        .is_some_and(|command| command.ends_with(HOOK_ARGS) && command.contains("rotter"))
}

fn read_settings(path: &Path) -> Result<Value, String> {
    match fs::read_to_string(path) {
        Ok(text) => {
            let value: Value = serde_json::from_str(&text)
                .map_err(|error| format!("cannot parse {}: {error}", path.display()))?;
            if value.is_object() {
                Ok(value)
            } else {
                Err(format!("{} is not a JSON object", path.display()))
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(error) => Err(format!("cannot read {}: {error}", path.display())),
    }
}

/// Writes through a temporary file so a failure never leaves a truncated settings.json.
fn write_settings(path: &Path, value: &Value) -> Result<(), String> {
    let text = serde_json::to_string_pretty(value).map_err(|error| error.to_string())? + "\n";
    let fail = |error: std::io::Error| format!("cannot write {}: {error}", path.display());
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(fail)?;
    }
    let old = fs::metadata(path).ok();
    if old.is_some() {
        fs::copy(path, path.with_extension("json.rotter-bak")).map_err(fail)?;
    }
    let temporary = path.with_extension("json.rotter-tmp");
    fs::write(&temporary, text).map_err(fail)?;
    if let Some(old) = old {
        fs::set_permissions(&temporary, old.permissions()).map_err(fail)?;
    }
    fs::rename(&temporary, path).map_err(fail)
}

/// Removes our Stop entries and returns how many were removed.
fn remove_ours(settings: &mut Value) -> usize {
    // get_mut, not IndexMut: indexing would insert `"hooks": null` into untouched settings.
    let Some(groups) = settings
        .get_mut("hooks")
        .and_then(|hooks| hooks.get_mut("Stop"))
        .and_then(Value::as_array_mut)
    else {
        return 0;
    };
    let mut removed = 0;
    for group in groups.iter_mut() {
        if let Some(hooks) = group["hooks"].as_array_mut() {
            let before = hooks.len();
            hooks.retain(|entry| !is_ours(entry));
            removed += before - hooks.len();
        }
    }
    groups.retain(|group| {
        group["hooks"]
            .as_array()
            .is_none_or(|hooks| !hooks.is_empty())
    });
    removed
}

fn target(name: &str) -> Result<(), String> {
    if name == "claude" {
        Ok(())
    } else {
        Err(format!("unknown integration {name:?}; available: claude"))
    }
}

pub fn install(name: &str) -> Result<String, String> {
    target(name)?;
    let path = claude_settings();
    let mut settings = read_settings(&path)?;
    let command = format!("{} {HOOK_ARGS}", shell_quote(&exe()));
    let current = settings["hooks"]["Stop"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|group| group["hooks"].as_array().into_iter().flatten())
        .filter(|entry| is_ours(entry))
        .map(|entry| entry["command"].as_str() == Some(command.as_str()))
        .collect::<Vec<_>>();
    if current == [true] {
        return Ok(format!("claude: already installed ({})", path.display()));
    }
    let replaced = remove_ours(&mut settings);
    if !settings["hooks"].is_object() {
        settings["hooks"] = json!({});
    }
    if !settings["hooks"]["Stop"].is_array() {
        settings["hooks"]["Stop"] = json!([]);
    }
    settings["hooks"]["Stop"]
        .as_array_mut()
        .expect("Stop is an array")
        .push(json!({ "hooks": [{ "type": "command", "command": command, "timeout": 60 }] }));
    write_settings(&path, &settings)?;
    let verb = if replaced > 0 { "updated" } else { "installed" };
    Ok(format!("claude: {verb} Stop hook in {}", path.display()))
}

pub fn uninstall(name: &str) -> Result<String, String> {
    target(name)?;
    let path = claude_settings();
    let mut settings = read_settings(&path)?;
    if remove_ours(&mut settings) == 0 {
        return Ok(format!("claude: not installed ({})", path.display()));
    }
    write_settings(&path, &settings)?;
    Ok(format!("claude: removed Stop hook from {}", path.display()))
}

pub fn status() -> Result<String, String> {
    let path = claude_settings();
    let settings = read_settings(&path)?;
    let command = format!("{} {HOOK_ARGS}", shell_quote(&exe()));
    let ours: Vec<&str> = settings["hooks"]["Stop"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|group| group["hooks"].as_array().into_iter().flatten())
        .filter(|entry| is_ours(entry))
        .filter_map(|entry| entry["command"].as_str())
        .collect();
    let state = match ours.as_slice() {
        [] => "not installed".to_owned(),
        [only] if *only == command => "installed (current)".to_owned(),
        _ => format!("installed for another binary: {}", ours.join(", ")),
    };
    Ok(format!("claude: {state} ({})", path.display()))
}
