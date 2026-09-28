use crate::config::{self, Config, absolute_var, home};
use crate::git::git_class;
use crate::grammar::{create_private, open_regular};
use crate::json::Json;
use crate::{Mode, Options, extract, toplevel};
use serde_json::{Value, json};
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Marks the settings entry this binary owns; everything else in settings.json is left alone.
const HOOK_ARGS: &str = "hook claude-stop";

/// Claude Code's default hook timeout, assumed when the installed entry has no usable one.
const DEFAULT_HOOK_TIMEOUT: u64 = 60;
/// Seconds of the hook timeout kept free for git and reporting after the per-file phases.
const HOOK_RESERVE: u64 = 15;

/// Where dedupe state lives; None (no absolute location) means the hook does not dedupe.
fn state_dir() -> Option<PathBuf> {
    absolute_var("ROTTER_STATE_DIR").or_else(|| {
        absolute_var("XDG_STATE_HOME")
            .or_else(|| home().map(|home| home.join(".local/state")))
            .map(|base| base.join("rotter"))
    })
}

fn claude_settings() -> Result<PathBuf, String> {
    absolute_var("CLAUDE_CONFIG_DIR")
        .or_else(|| home().map(|home| home.join(".claude")))
        .map(|dir| dir.join("settings.json"))
        .ok_or_else(|| {
            "cannot locate Claude Code settings: set HOME or an absolute CLAUDE_CONFIG_DIR"
                .to_owned()
        })
}

/// The hook `timeout` to install: room for one full parse plus git and reporting.
pub fn hook_timeout(parse_timeout_seconds: u64) -> u64 {
    DEFAULT_HOOK_TIMEOUT.max(parse_timeout_seconds.saturating_add(30))
}

/// Smallest positive integer `timeout` among entries with exactly this command, else 60.
fn installed_timeout(settings: &Value, command: &str) -> u64 {
    our_entries(settings)
        .filter(|entry| entry["command"].as_str() == Some(command))
        .filter_map(|entry| entry["timeout"].as_u64().filter(|timeout| *timeout > 0))
        .min()
        .unwrap_or(DEFAULT_HOOK_TIMEOUT)
}

/// Soft budget for the per-file phases of one hook run.
fn run_budget(computed: u64, installed: u64) -> Duration {
    Duration::from_secs(computed.min(installed).saturating_sub(HOOK_RESERVE))
}

/// The installed timeout from the user settings.json; a missing or non-regular file counts as 60.
fn settings_timeout(command: &str) -> u64 {
    claude_settings()
        .ok()
        .and_then(|path| read_settings(&path).ok())
        .map_or(DEFAULT_HOOK_TIMEOUT, |(settings, _)| {
            installed_timeout(&settings, command)
        })
}

fn hook_command() -> String {
    format!("{} {HOOK_ARGS}", shell_quote(&exe()))
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
    let start = Instant::now();
    let input: Value = serde_json::from_str(input).unwrap_or(Value::Null);
    // Claude Code sends stop_hook_active; Grok Build sends stopHookActive.
    if ["stop_hook_active", "stopHookActive"]
        .iter()
        .any(|key| input[key] == Value::Bool(true))
    {
        return None;
    }
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
    let state = state_dir();
    // Too old or unrecognised git: no repository is looked at, not even to find its top level.
    // One note per session; the refusal is only rendered here, per host. Without git on the
    // absolute PATH entries there is nothing to do, silently, as before.
    crate::install::git_program().ok()?;
    if let Err(error) = git_class() {
        let repeated = state
            .as_ref()
            .is_some_and(|dir| !first_time(&dir.join("claude-stop-errors"), &session, &error));
        return (!repeated)
            .then(|| json!({ "systemMessage": format!("rotter: {error}") }).to_string());
    }
    let cwd = input["cwd"]
        .as_str()
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())?;
    // Outside a work tree there is nothing to do.
    let top = toplevel(&cwd).ok()?;
    // Config problems never block: they become one systemMessage per session and builtins are
    // used.
    let mut notes = Vec::new();
    let config = match config::load(Some(&top)) {
        Ok(loaded) => {
            notes.extend(loaded.note);
            loaded.config
        }
        Err(error) => {
            notes.push(format!(
                "config error: {error}; external languages disabled"
            ));
            Config::default()
        }
    };
    if !notes.is_empty()
        && let Some(dir) = &state
        && !first_time(&dir.join("claude-stop-notes"), &session, &notes.join("\n"))
    {
        notes.clear();
    }
    let message = |mut notes: Vec<String>, text: Option<String>| {
        notes.extend(text);
        (!notes.is_empty()).then(|| {
            json!({ "systemMessage": format!("rotter: {}", notes.join("; ")) }).to_string()
        })
    };
    let rotter = shell_quote(&exe());
    let command = format!(
        "{rotter} extract --worktree --include-untracked -C {}",
        shell_quote(&cwd.display().to_string())
    );
    let budget = run_budget(
        hook_timeout(config.parse_timeout_seconds),
        settings_timeout(&hook_command()),
    );
    let mut options = Options::new(Mode::Worktree);
    options.include_untracked = true;
    options.grammars = config.languages();
    options.languages = config.override_languages(&options.grammars);
    options.parse_timeout = config.parse_timeout();
    options.deadline = start.checked_add(budget);
    let report = match extract(&cwd, &options) {
        Ok(report) => report,
        Err(error) => {
            // Persistent failures (e.g. a refused TMPDIR) would otherwise repeat on every Stop.
            let text = format!("extract failed: {error}");
            let repeated = state
                .as_ref()
                .is_some_and(|dir| !first_time(&dir.join("claude-stop-errors"), &session, &text));
            return message(notes, (!repeated).then_some(text));
        }
    };
    let found = units(&report.json);
    if found == 0 && report.complete {
        return message(notes, None);
    }

    let text = report.json.to_string();
    // Incomplete reports without units are recorded too, so each is announced once.
    if let Some(dir) = &state
        && !first_time(
            &dir.join("claude-stop"),
            &session,
            &format!("{}\0{text}", cwd.display()),
        )
    {
        return message(notes, None);
    }
    if found == 0 {
        return message(
            notes,
            Some(format!(
                "some changed files could not be analysed; run {command}"
            )),
        );
    }
    let reason = format!(
        "rotter found {found} changed code unit(s) with related comments in {} (report complete: {}). \
         Before finishing, review them with the rotter-comment-review skill in working tree mode \
         including untracked files: run `{command}`. If that skill is not loaded, run \
         `{rotter} --skill` and follow its output. Report only concrete contradictions between \
         comments and code; do not edit files unless the user asked for it.",
        cwd.display(),
        report.complete
    );
    let mut output = json!({ "decision": "block", "reason": reason });
    if !notes.is_empty() {
        output["systemMessage"] = format!("rotter: {}", notes.join("; ")).into();
    }
    Some(output.to_string())
}

/// Records `content`'s fingerprint as the latest seen in `dir/session`; false when it already was.
/// Failing to record state only means the same content may be announced again.
fn first_time(dir: &Path, session: &str, content: &str) -> bool {
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    let fingerprint = format!("{:016x}", hasher.finish());
    let state = dir.join(session);
    if fs::read_to_string(&state).is_ok_and(|seen| seen.trim() == fingerprint) {
        return false;
    }
    let _ = fs::create_dir_all(dir).and_then(|()| fs::write(&state, format!("{fingerprint}\n")));
    true
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

fn is_ours(entry: &Value) -> bool {
    entry["command"]
        .as_str()
        .is_some_and(|command| command.ends_with(HOOK_ARGS) && command.contains("rotter"))
}

/// settings.json and its text; only a regular file is read (lstat first, so a FIFO or device
/// fails instead of blocking). A missing file is an empty object with no text.
fn read_settings(path: &Path) -> Result<(Value, Option<String>), String> {
    let Some(mut file) = open_regular(path)? else {
        return Ok((json!({}), None));
    };
    let mut text = String::new();
    file.read_to_string(&mut text)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let value: Value = serde_json::from_str(&text)
        .map_err(|error| format!("cannot parse {}: {error}", path.display()))?;
    if value.is_object() {
        Ok((value, Some(text)))
    } else {
        Err(format!("{} is not a JSON object", path.display()))
    }
}

/// Removes a stale regular file at `path`; any other type is an error and is left alone.
fn remove_stale(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => fs::remove_file(path)
            .map_err(|error| format!("cannot remove {}: {error}", path.display())),
        Ok(_) => Err(format!(
            "{} exists and is not a regular file; remove it yourself",
            path.display()
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

fn backup_path(path: &Path) -> PathBuf {
    path.with_extension("json.rotter-bak")
}

/// Writes `settings.json.rotter-bak` (0600) from the bytes already read.
fn write_backup(path: &Path, text: &str) -> Result<(), String> {
    let backup = backup_path(path);
    remove_stale(&backup)?;
    create_private(&backup)
        .and_then(|mut file| file.write_all(text.as_bytes()))
        .map_err(|error| format!("cannot write {}: {error}", backup.display()))
}

/// Writes through a temporary file so a failure never leaves a truncated settings.json. The
/// temporary is created 0600 without following symlinks and gets the original's permission bits
/// through its descriptor before the rename.
fn write_settings(path: &Path, value: &Value) -> Result<(), String> {
    let text = serde_json::to_string_pretty(value).map_err(|error| error.to_string())? + "\n";
    let temporary = path.with_extension("json.rotter-tmp");
    let fail = |error: std::io::Error| format!("cannot write {}: {error}", temporary.display());
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }
    let mode = fs::symlink_metadata(path)
        .ok()
        .filter(fs::Metadata::is_file)
        .map(|meta| meta.permissions().mode() & 0o777);
    remove_stale(&temporary)?;
    let mut file = create_private(&temporary).map_err(fail)?;
    file.write_all(text.as_bytes()).map_err(fail)?;
    if let Some(mode) = mode {
        file.set_permissions(fs::Permissions::from_mode(mode))
            .map_err(fail)?;
    }
    drop(file);
    fs::rename(&temporary, path)
        .map_err(|error| format!("cannot replace {}: {error}", path.display()))
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

/// Every Stop hook entry that belongs to some rotter binary.
fn our_entries(settings: &Value) -> impl Iterator<Item = &Value> {
    settings["hooks"]["Stop"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|group| group["hooks"].as_array().into_iter().flatten())
        .filter(|entry| is_ours(entry))
}

/// Installs the Stop hook with `timeout` sized for `parse_timeout_seconds`.
pub fn install(name: &str, parse_timeout_seconds: u64) -> Result<String, String> {
    target(name)?;
    let path = claude_settings()?;
    let (mut settings, original) = read_settings(&path)?;
    let command = hook_command();
    let timeout = hook_timeout(parse_timeout_seconds);
    // Current only when both command and timeout match; otherwise it is replaced.
    let current = our_entries(&settings)
        .map(|entry| {
            entry["command"].as_str() == Some(command.as_str())
                && entry["timeout"].as_u64() == Some(timeout)
        })
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
        .push(json!({ "hooks": [{ "type": "command", "command": command, "timeout": timeout }] }));
    if let Some(original) = &original {
        write_backup(&path, original)?;
    }
    write_settings(&path, &settings)?;
    let verb = if replaced > 0 { "updated" } else { "installed" };
    Ok(format!("claude: {verb} Stop hook in {}", path.display()))
}

pub fn uninstall(name: &str) -> Result<String, String> {
    target(name)?;
    let path = claude_settings()?;
    // An unreadable or unparseable settings.json stops here and keeps any backup.
    let (mut settings, _) = read_settings(&path)?;
    let mut lines = vec![if remove_ours(&mut settings) == 0 {
        format!("claude: not installed ({})", path.display())
    } else {
        write_settings(&path, &settings)?;
        format!("claude: removed Stop hook from {}", path.display())
    }];
    // unlink never follows a symlink, and only a regular file is removed at all.
    let backup = backup_path(&path);
    match fs::symlink_metadata(&backup) {
        Ok(meta) if meta.is_file() => {
            fs::remove_file(&backup)
                .map_err(|error| format!("cannot remove {}: {error}", backup.display()))?;
            lines.push(format!("claude: removed {}", backup.display()));
        }
        Ok(_) => lines.push(format!(
            "claude: left {} alone: it is not a regular file",
            backup.display()
        )),
        Err(_) => {}
    }
    Ok(lines.join("\n"))
}

/// Reports the hook state; `parse_timeout_seconds` gives the expected timeout.
pub fn status(parse_timeout_seconds: u64) -> Result<String, String> {
    let path = claude_settings()?;
    let (settings, _) = read_settings(&path)?;
    let command = hook_command();
    let expected = hook_timeout(parse_timeout_seconds);
    let ours: Vec<&Value> = our_entries(&settings).collect();
    let state = match ours.as_slice() {
        [] => "not installed".to_owned(),
        [only] if only["command"].as_str() == Some(command.as_str()) => {
            let timeout = &only["timeout"];
            if timeout.as_u64() == Some(expected) {
                "installed (current)".to_owned()
            } else {
                let timeout = match timeout {
                    Value::Null => "no timeout".to_owned(),
                    value if value.is_u64() => format!("timeout {value}"),
                    Value::Number(value) => {
                        format!("timeout {value} (not a positive whole number of seconds)")
                    }
                    value => format!("timeout {value} (not a number)"),
                };
                format!(
                    "installed ({timeout}, expected {expected}); run `rotter integration install claude`"
                )
            }
        }
        _ => format!(
            "installed for another binary: {}",
            ours.iter()
                .filter_map(|entry| entry["command"].as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };
    Ok(format!("claude: {state} ({})", path.display()))
}

#[cfg(test)]
mod tests {
    use super::{hook_timeout, installed_timeout, run_budget};
    use serde_json::{Value, json};
    use std::time::Duration;

    const COMMAND: &str = "'/bin/rotter' hook claude-stop";

    fn settings(timeouts: &[Value]) -> Value {
        let hooks: Vec<Value> = timeouts
            .iter()
            .map(|timeout| json!({ "type": "command", "command": COMMAND, "timeout": timeout }))
            .collect();
        json!({ "hooks": { "Stop": [{ "hooks": hooks }] } })
    }

    #[test]
    fn run_budget_uses_the_smaller_timeout_minus_the_reserve() {
        let default = hook_timeout(60);
        assert_eq!(default, 90);
        let budget = |timeouts: &[Value]| {
            run_budget(default, installed_timeout(&settings(timeouts), COMMAND))
        };
        assert_eq!(budget(&[json!(10)]), Duration::ZERO);
        assert_eq!(
            budget(&[json!(0)]),
            Duration::from_secs(45),
            "0 counts as unset"
        );
        assert_eq!(budget(&[json!(60)]), Duration::from_secs(45));
        assert_eq!(budget(&[json!(90)]), Duration::from_secs(75));
        assert_eq!(budget(&[json!("90")]), Duration::from_secs(45));
        assert_eq!(budget(&[json!(-5)]), Duration::from_secs(45));
        assert_eq!(budget(&[json!(90.5)]), Duration::from_secs(45));
        assert_eq!(budget(&[]), Duration::from_secs(45), "missing entry");
        assert_eq!(
            budget(&[json!(90), json!(30)]),
            Duration::from_secs(15),
            "smallest wins"
        );
        let other = json!({ "hooks": { "Stop": [{ "hooks": [
            { "type": "command", "command": "'/other/rotter' hook claude-stop", "timeout": 5 }
        ] }] } });
        assert_eq!(
            installed_timeout(&other, COMMAND),
            60,
            "another binary's entry"
        );
        assert_eq!(
            run_budget(
                hook_timeout(300),
                installed_timeout(&settings(&[json!(90)]), COMMAND)
            ),
            Duration::from_secs(75)
        );
        assert_eq!(hook_timeout(300), 330);
        assert_eq!(hook_timeout(1), 60);
    }
}
