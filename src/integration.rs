use crate::config::{
    self, Config, Kind, Meta, Untrusted, absolute_var, home, lstat, resolve_trusted, trusted_file,
    user,
};
use crate::git::git_class;
use crate::grammar::{create_private, open_regular};
use crate::json::Json;
use crate::{Mode, Options, extract, toplevel};
use serde_json::{Value, json};
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Claude Code's default hook timeout, assumed when the installed entry has no usable one.
const DEFAULT_HOOK_TIMEOUT: u64 = 60;
/// Grok Build's default Stop hook timeout, assumed when rotter.json has no usable one.
const GROK_HOOK_TIMEOUT: u64 = 600;
/// Seconds of the hook timeout kept free for git and reporting after the per-file phases.
const HOOK_RESERVE: u64 = 15;

/// A host that runs rotter as a Stop hook.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Host {
    Claude,
    Grok,
}

impl Host {
    const ALL: [Self; 2] = [Self::Claude, Self::Grok];

    fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Grok => "grok",
        }
    }

    /// The host of `rotter hook <args>`; None for anything else.
    pub fn from_hook(args: &[&str]) -> Option<Self> {
        match args {
            ["claude-stop"] => Some(Self::Claude),
            ["grok-stop"] => Some(Self::Grok),
            _ => None,
        }
    }

    /// `'<exe>' hook <host>-stop || true`: a missing or older binary can never exit 2 and so
    /// never forces continuations.
    fn command(self, exe: &str) -> String {
        format!("{} hook {}-stop || true", shell_quote(exe), self.name())
    }
}

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

/// `$GROK_HOME` when absolute, else `$HOME/.grok`.
fn grok_home() -> Result<PathBuf, String> {
    absolute_var("GROK_HOME")
        .or_else(|| home().map(|home| home.join(".grok")))
        .ok_or_else(|| {
            "cannot locate Grok Build's home: set HOME or an absolute GROK_HOME".to_owned()
        })
}

/// The hook `timeout` to install: room for one full parse plus git and reporting.
pub fn hook_timeout(parse_timeout_seconds: u64) -> u64 {
    DEFAULT_HOOK_TIMEOUT.max(parse_timeout_seconds.saturating_add(30))
}

/// Smallest positive integer `timeout` among Stop entries with exactly this command.
fn entry_timeout(settings: &Value, command: &str) -> Option<u64> {
    stop_entries(settings)
        .filter(|entry| entry["command"].as_str() == Some(command))
        .filter_map(|entry| entry["timeout"].as_u64().filter(|timeout| *timeout > 0))
        .min()
}

/// [`entry_timeout`], else Claude Code's 60.
fn installed_timeout(settings: &Value, command: &str) -> u64 {
    entry_timeout(settings, command).unwrap_or(DEFAULT_HOOK_TIMEOUT)
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

/// The installed timeout from rotter.json, read like settings.json (never followed, no FIFO
/// block); anything unusable counts as Grok's 600.
fn grok_timeout(command: &str) -> u64 {
    grok_home()
        .ok()
        .and_then(|home| read_settings(&home.join("hooks/rotter.json")).ok())
        .and_then(|(settings, _)| entry_timeout(&settings, command))
        .unwrap_or(GROK_HOOK_TIMEOUT)
}

fn exe() -> String {
    std::env::current_exe().map_or_else(|_| "rotter".to_owned(), |path| path.display().to_string())
}

/// This binary's path for a hook command. The file and every directory to it must pass
/// `trusted_file`, and the path must not contain `$` or `` ` `` (Grok expands `$VAR` in
/// commands), NUL or a newline.
fn trusted_exe() -> Result<String, String> {
    let path =
        std::env::current_exe().map_err(|error| format!("cannot locate this binary: {error}"))?;
    let text = path
        .to_str()
        .ok_or_else(|| format!("refusing to register {}: not UTF-8", path.display()))?;
    if text.contains(['$', '`', '\0', '\n']) {
        return Err(format!(
            "refusing to register {text}: its path contains $, `, NUL or a newline"
        ));
    }
    match trusted_file(&path, user(), &lstat) {
        Ok(_) => Ok(text.to_owned()),
        Err(Untrusted::Missing) => Err(format!("refusing to register {text}: it does not exist")),
        Err(Untrusted::Refused(why)) => Err(format!("refusing to register {text}: {why}")),
    }
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

/// What one Stop run found: messages that do not block, the review request, and the host's
/// "last blocked" slot to record once the request is on stdout.
#[derive(Default)]
struct Outcome {
    messages: Vec<String>,
    reason: Option<String>,
    record: Option<(PathBuf, String)>,
}

/// A Stop hook run for `host`. Claude gets the decision and any messages as JSON on stdout;
/// Grok gets only a decision on stdout and the messages on stderr. Never fails.
pub fn stop(host: Host, input: &str) {
    let outcome = evaluate(host, input).unwrap_or_default();
    let messages =
        (!outcome.messages.is_empty()).then(|| format!("rotter: {}", outcome.messages.join("; ")));
    let output = match (outcome.reason, host) {
        (Some(reason), _) => {
            let mut output = json!({ "decision": "block", "reason": reason });
            if host == Host::Claude
                && let Some(messages) = &messages
            {
                output["systemMessage"] = messages.clone().into();
            }
            Some(output)
        }
        (None, Host::Claude) => messages
            .as_ref()
            .map(|messages| json!({ "systemMessage": messages })),
        (None, Host::Grok) => None,
    };
    if host == Host::Grok
        && let Some(messages) = &messages
    {
        eprintln!("{messages}");
    }
    if let Some(output) = output {
        let mut stdout = io::stdout().lock();
        // The slot is written only once the request has reached the host.
        if writeln!(stdout, "{output}")
            .and_then(|()| stdout.flush())
            .is_ok()
            && let Some((slot, fingerprint)) = outcome.record
        {
            record(&slot, &fingerprint);
        }
    }
}

/// Blocks once when the working tree has changes that relate to comments, never blocks a
/// continuation it caused, and asks again only after the report changes.
fn evaluate(host: Host, input: &str) -> Option<Outcome> {
    let start = Instant::now();
    let input: Value = serde_json::from_str(input).unwrap_or(Value::Null);
    // Claude Code sends stop_hook_active; Grok Build sends stopHookActive.
    if ["stop_hook_active", "stopHookActive"]
        .iter()
        .any(|key| input[key] == Value::Bool(true))
    {
        return None;
    }
    // Grok's session-end Stop (`channel_closed`, `shutdown`) ends no turn; its decision is ignored.
    if input
        .get("reason")
        .is_some_and(|reason| reason != "end_turn")
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
    // Claude shows each diagnostic once per session. Grok does not show stderr, so its notes are
    // never marked as announced and recur, harmlessly.
    let announce = |store: &str, text: &str| {
        host == Host::Grok
            || state.as_ref().is_none_or(|dir| {
                first_time(
                    &dir.join(format!("{}-stop-{store}", host.name())),
                    &session,
                    text,
                )
            })
    };
    let mut outcome = Outcome::default();
    // Too old or unrecognised git: no repository is looked at, not even to find its top level.
    // The refusal is only rendered here, per host. Without git on the absolute PATH entries
    // there is nothing to do, silently, as before.
    crate::install::git_program().ok()?;
    if let Err(error) = git_class() {
        if announce("errors", &error) {
            outcome.messages.push(error);
        }
        return Some(outcome);
    }
    let cwd = match host {
        Host::Claude => input["cwd"]
            .as_str()
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())?,
        Host::Grok => input["cwd"]
            .as_str()
            .or(input["workspaceRoot"].as_str())
            .filter(|cwd| cwd.starts_with('/') && !cwd.contains(char::is_control))
            .map(PathBuf::from)?,
    };
    // Outside a work tree there is nothing to do.
    let top = toplevel(&cwd).ok()?;
    // Config problems never block: they are announced and builtins are used.
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
    if !notes.is_empty() && announce("notes", &notes.join("\n")) {
        outcome.messages.extend(notes);
    }
    let rotter = shell_quote(&exe());
    let command = format!(
        "{rotter} extract --worktree --include-untracked -C {}",
        shell_quote(&cwd.display().to_string())
    );
    let installed = match host {
        Host::Claude => settings_timeout(&host.command(&exe())),
        Host::Grok => grok_timeout(&host.command(&exe())),
    };
    let budget = run_budget(hook_timeout(config.parse_timeout_seconds), installed);
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
            if announce("errors", &text) {
                outcome.messages.push(text);
            }
            return Some(outcome);
        }
    };
    let found = units(&report.json);
    if found == 0 && report.complete {
        return Some(outcome);
    }
    // Keyed on the top level, so hosts deriving cwd differently share fingerprints.
    let key = format!("{}\0{}", top.display(), report.json);
    match verdict(state.as_deref(), host, &session, &key, found) {
        Verdict::Quiet => {}
        Verdict::Unanalysed => {
            if announce("errors", &key) {
                outcome.messages.push(format!(
                    "some changed files could not be analysed; run {command}"
                ));
            }
        }
        Verdict::Block(record) => {
            outcome.reason = Some(format!(
                "rotter found {found} changed code unit(s) with related comments in {} (report complete: {}). \
                 Before finishing, review them with the rotter-comment-review skill in working tree mode \
                 including untracked files: run `{command}`. If that skill is not loaded, run \
                 `{rotter} --skill` and follow its output. Report only concrete contradictions between \
                 comments and code; do not edit files unless the user asked for it.",
                cwd.display(),
                report.complete
            ));
            outcome.record = record;
        }
    }
    Some(outcome)
}

#[derive(Debug, PartialEq)]
enum Verdict {
    /// Every host's slot is checked: this report was the last block of some host.
    Quiet,
    /// No units, only an incomplete report: a diagnostic, which never touches the slots.
    Unanalysed,
    /// A review request and the emitting host's slot to record after it is printed (None
    /// without a state directory).
    Block(Option<(PathBuf, String)>),
}

/// What a Stop does with a changed report `key` holding `found` units. Each host keeps one "last
/// blocked" fingerprint per session; a report equal to any host's is quiet, so a native Grok
/// hook and its Claude-compat twin ask once for the same report, while a report that changes and
/// later returns may be requested again. Concurrent runs may both ask (accepted).
fn verdict(state: Option<&Path>, host: Host, session: &str, key: &str, found: usize) -> Verdict {
    if found == 0 {
        return Verdict::Unanalysed;
    }
    let Some(state) = state else {
        return Verdict::Block(None);
    };
    let fingerprint = fingerprint(key);
    if Host::ALL
        .iter()
        .any(|other| seen(&slot(state, *other, session)).as_deref() == Some(fingerprint.as_str()))
    {
        return Verdict::Quiet;
    }
    Verdict::Block(Some((slot(state, host, session), fingerprint)))
}

/// `<state>/<host>-stop/<session>`: the host's last blocked fingerprint.
fn slot(state: &Path, host: Host, session: &str) -> PathBuf {
    state.join(format!("{}-stop", host.name())).join(session)
}

fn fingerprint(content: &str) -> String {
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn seen(path: &Path) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|seen| seen.trim().to_owned())
}

/// Failing to record state only means the same content may be announced again.
fn record(path: &Path, fingerprint: &str) {
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir).and_then(|()| fs::write(path, format!("{fingerprint}\n")));
    }
}

/// Records `content`'s fingerprint as the latest seen in `dir/session`; false when it already was.
fn first_time(dir: &Path, session: &str, content: &str) -> bool {
    let fingerprint = fingerprint(content);
    let state = dir.join(session);
    if seen(&state).as_deref() == Some(fingerprint.as_str()) {
        return false;
    }
    record(&state, &fingerprint);
    true
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// A Claude entry of some rotter binary, in the current (`… || true`) or the older form.
fn is_ours(entry: &Value) -> bool {
    entry["command"].as_str().is_some_and(|command| {
        (command.ends_with("hook claude-stop") || command.ends_with("hook claude-stop || true"))
            && command.contains("rotter")
    })
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
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
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

/// Replaces `path` with `text` through `<name>.json.rotter-tmp`, so a failure never leaves a
/// truncated file (a stale regular temporary is removed first). The temporary is created 0600
/// without following symlinks and gets `mode` through its descriptor before the rename.
fn replace_file(path: &Path, text: &str, mode: Option<u32>) -> Result<(), String> {
    let temporary = path.with_extension("json.rotter-tmp");
    let fail = |error: io::Error| format!("cannot write {}: {error}", temporary.display());
    remove_stale(&temporary)?;
    let mut file = create_private(&temporary).map_err(fail)?;
    if let Some(mode) = mode {
        file.set_permissions(fs::Permissions::from_mode(mode))
            .map_err(fail)?;
    }
    file.write_all(text.as_bytes()).map_err(fail)?;
    drop(file);
    fs::rename(&temporary, path)
        .map_err(|error| format!("cannot replace {}: {error}", path.display()))
}

/// Writes settings.json atomically, keeping the original's permission bits.
fn write_settings(path: &Path, value: &Value) -> Result<(), String> {
    let text = serde_json::to_string_pretty(value).map_err(|error| error.to_string())? + "\n";
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }
    let mode = fs::symlink_metadata(path)
        .ok()
        .filter(fs::Metadata::is_file)
        .map(|meta| meta.permissions().mode() & 0o777);
    replace_file(path, &text, mode)
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

fn target(name: &str) -> Result<Host, String> {
    Host::ALL
        .into_iter()
        .find(|host| host.name() == name)
        .ok_or_else(|| format!("unknown integration {name:?}; available: claude, grok"))
}

/// Every Stop hook entry, in any group.
fn stop_entries(settings: &Value) -> impl Iterator<Item = &Value> {
    settings["hooks"]["Stop"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|group| group["hooks"].as_array().into_iter().flatten())
}

/// Every Claude Stop hook entry that belongs to some rotter binary.
fn our_entries(settings: &Value) -> impl Iterator<Item = &Value> {
    stop_entries(settings).filter(|entry| is_ours(entry))
}

/// A directory owned by the user (root is not accepted) that group and others cannot write.
fn owned_dir(path: &Path, meta: Meta, user: u32) -> Result<(), String> {
    if meta.kind == Kind::Dir && meta.uid == user && meta.mode & 0o022 == 0 {
        Ok(())
    } else {
        Err(format!(
            "{} must be a directory owned by you that group and others cannot write",
            path.display()
        ))
    }
}

/// `home` through `resolve_trusted`, then [`owned_dir`]; None when it does not exist.
fn trusted_home(
    home: &Path,
    user: u32,
    lstat: &dyn Fn(&Path) -> io::Result<Meta>,
) -> Result<Option<PathBuf>, String> {
    let resolved = match resolve_trusted(home, user, lstat) {
        Ok(resolved) => resolved,
        Err(Untrusted::Missing) => return Ok(None),
        Err(Untrusted::Refused(why)) => {
            return Err(format!("Grok home {} refused: {why}", home.display()));
        }
    };
    let meta = lstat(&resolved).map_err(|error| format!("{}: {error}", resolved.display()))?;
    owned_dir(&resolved, meta, user)?;
    Ok(Some(resolved))
}

/// `<grok home>/hooks/rotter.json` as configured (not resolved), for messages only.
fn grok_display() -> Result<PathBuf, String> {
    Ok(grok_home()?.join("hooks/rotter.json"))
}

/// `<resolved grok home>/hooks/rotter.json`, with `hooks/` checked by [`owned_dir`] (lstat, so
/// never a symlink); None when the home or `hooks/` does not exist. With `create`, a missing home
/// is an error and a missing `hooks/` is created at 0700 (never its parents).
fn grok_file(create: bool) -> Result<Option<PathBuf>, String> {
    let home = grok_home()?;
    let Some(resolved) = trusted_home(&home, user(), &lstat)? else {
        return if create {
            Err(format!(
                "{} does not exist; start Grok Build once or create it",
                home.display()
            ))
        } else {
            Ok(None)
        };
    };
    let hooks = resolved.join("hooks");
    let fail = |error: io::Error| format!("{}: {error}", hooks.display());
    let meta = match lstat(&hooks) {
        Ok(meta) => meta,
        Err(error) if error.kind() == io::ErrorKind::NotFound && create => {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(&hooks)
                .map_err(|error| format!("cannot create {}: {error}", hooks.display()))?;
            lstat(&hooks).map_err(fail)?
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(fail(error)),
    };
    owned_dir(&hooks, meta, user())?;
    Ok(Some(hooks.join("rotter.json")))
}

/// The only handler of a rotter-generated Grok document, which parses to exactly
/// `{"hooks":{"Stop":[{"hooks":[{"type":"command","command":C,"timeout":N}]}]}}` with C
/// `'<absolute path>' hook grok-stop || true` and N a positive integer; None for anything else.
fn grok_handler(document: &Value) -> Option<&Value> {
    fn only<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
        let object = value.as_object()?;
        if object.len() == 1 {
            object.get(key)
        } else {
            None
        }
    }
    let [group] = only(only(document, "hooks")?, "Stop")?
        .as_array()?
        .as_slice()
    else {
        return None;
    };
    let [handler] = only(group, "hooks")?.as_array()?.as_slice() else {
        return None;
    };
    let fields = handler.as_object()?;
    let quoted = fields
        .get("command")?
        .as_str()?
        .strip_suffix(" hook grok-stop || true")?;
    let path = quoted
        .strip_prefix('\'')?
        .strip_suffix('\'')?
        .replace(r"'\''", "'");
    (fields.len() == 3
        && fields.get("type")? == "command"
        && fields
            .get("timeout")?
            .as_u64()
            .is_some_and(|timeout| timeout > 0)
        && path.starts_with('/')
        && shell_quote(&path) == quoted)
        .then_some(handler)
}

/// The handler of an installed rotter.json; None when there is none. It must be a regular file
/// (never followed, no FIFO block) owned by the user that group and others cannot write, holding
/// a rotter-generated document; anything else is an error and the file is left alone.
fn grok_installed(path: &Path) -> Result<Option<Value>, String> {
    let Some(mut file) = open_regular(path)? else {
        return Ok(None);
    };
    let fail = |error: io::Error| format!("cannot read {}: {error}", path.display());
    let meta = file.metadata().map_err(fail)?;
    if meta.uid() != user() || meta.mode() & 0o022 != 0 {
        return Err(format!(
            "{} has unsafe permissions (it must be yours and not writable by group or others); \
             left alone",
            path.display()
        ));
    }
    let mut text = String::new();
    file.read_to_string(&mut text).map_err(fail)?;
    serde_json::from_str(&text)
        .ok()
        .as_ref()
        .and_then(grok_handler)
        .map(|handler| Some(handler.clone()))
        .ok_or_else(|| format!("{} is not managed by rotter; left alone", path.display()))
}

/// Installs the Stop hook with `timeout` sized for `parse_timeout_seconds`.
pub fn install(name: &str, parse_timeout_seconds: u64) -> Result<String, String> {
    let host = target(name)?;
    // Checked before anything is written: the host runs this path on every Stop.
    let command = host.command(&trusted_exe()?);
    let timeout = hook_timeout(parse_timeout_seconds);
    match host {
        Host::Claude => install_claude(&command, timeout),
        Host::Grok => install_grok(&command, timeout),
    }
}

fn install_claude(command: &str, timeout: u64) -> Result<String, String> {
    let path = claude_settings()?;
    let (mut settings, original) = read_settings(&path)?;
    // Current only when both command and timeout match; otherwise (including the older command
    // form) it is replaced.
    let current = our_entries(&settings)
        .map(|entry| {
            entry["command"].as_str() == Some(command) && entry["timeout"].as_u64() == Some(timeout)
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

/// Writes `<grok home>/hooks/rotter.json`, which rotter owns entirely (no backup).
fn install_grok(command: &str, timeout: u64) -> Result<String, String> {
    let path = grok_file(true)?.ok_or("the Grok hooks directory vanished")?;
    let installed = grok_installed(&path)?;
    if let Some(handler) = &installed
        && handler["command"] == command
        && handler["timeout"] == timeout
    {
        return Ok(format!("grok: already installed ({})", path.display()));
    }
    let document = json!({ "hooks": { "Stop": [{ "hooks": [
        { "type": "command", "command": command, "timeout": timeout }
    ] }] } });
    let text = serde_json::to_string_pretty(&document).map_err(|error| error.to_string())? + "\n";
    // Exactly 0600 whatever the umask; nothing is copied from the old file.
    replace_file(&path, &text, Some(0o600))?;
    let verb = if installed.is_some() {
        "updated"
    } else {
        "installed"
    };
    Ok(format!("grok: {verb} Stop hook in {}", path.display()))
}

pub fn uninstall(name: &str) -> Result<String, String> {
    match target(name)? {
        Host::Claude => uninstall_claude(),
        Host::Grok => uninstall_grok(),
    }
}

fn uninstall_claude() -> Result<String, String> {
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

/// Unlinks rotter.json only when [`grok_installed`] accepts it.
fn uninstall_grok() -> Result<String, String> {
    let Some(path) = grok_file(false)? else {
        return Ok(format!(
            "grok: not installed ({})",
            grok_display()?.display()
        ));
    };
    if grok_installed(&path)?.is_none() {
        return Ok(format!("grok: not installed ({})", path.display()));
    }
    fs::remove_file(&path).map_err(|error| format!("cannot remove {}: {error}", path.display()))?;
    Ok(format!("grok: removed Stop hook {}", path.display()))
}

/// One host's state from its rotter entries.
fn describe(host: Host, ours: &[&Value], command: &str, expected: u64) -> String {
    let install = format!("run `rotter integration install {}`", host.name());
    match ours {
        [] => "not installed".to_owned(),
        [only] if only["command"].as_str() == Some(command) => {
            let timeout = &only["timeout"];
            if timeout.as_u64() == Some(expected) {
                return "installed (current)".to_owned();
            }
            let timeout = match timeout {
                Value::Null => "no timeout".to_owned(),
                value if value.is_u64() => format!("timeout {value}"),
                Value::Number(value) => {
                    format!("timeout {value} (not a positive whole number of seconds)")
                }
                value => format!("timeout {value} (not a number)"),
            };
            format!("installed ({timeout}, expected {expected}); {install}")
        }
        [only] if only["command"].as_str() == command.strip_suffix(" || true") => {
            format!("installed (older command without `|| true`); {install}")
        }
        _ => format!(
            "installed for another binary: {}",
            ours.iter()
                .filter_map(|entry| entry["command"].as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn grok_status(command: &str, expected: u64) -> Result<String, String> {
    let Some(path) = grok_file(false)? else {
        return Ok(format!("not installed ({})", grok_display()?.display()));
    };
    let handler = grok_installed(&path)?;
    let state = describe(Host::Grok, &Vec::from_iter(&handler), command, expected);
    Ok(format!("{state} ({})", path.display()))
}

/// Whether Grok's Claude compatibility, which reads the literal `$HOME/.claude/settings.json`,
/// can pick up a rotter entry. Read-only and error-tolerant: anything unusual is "unknown".
/// Whether compat is enabled (`[compat.claude]`, `GROK_CLAUDE_HOOKS_ENABLED`) is not evaluated.
fn compat_status() -> Option<String> {
    let Some(home) = home() else {
        return Some("grok: Claude compatibility entry unknown: HOME is not set".to_owned());
    };
    let path = home.join(".claude/settings.json");
    match read_settings(&path) {
        Ok((settings, _)) => our_entries(&settings).next().map(|_| {
            format!(
                "grok: found a Claude entry that Grok's Claude compatibility can pick up (whether \
                 compat is enabled was not checked) ({})",
                path.display()
            )
        }),
        Err(why) => Some(format!("grok: Claude compatibility entry unknown: {why}")),
    }
}

/// Reports each host's hook state; `parse_timeout_seconds` gives the expected timeout.
pub fn status(parse_timeout_seconds: u64) -> Result<String, String> {
    let exe = exe();
    let expected = hook_timeout(parse_timeout_seconds);
    let path = claude_settings()?;
    let (settings, _) = read_settings(&path)?;
    let ours: Vec<&Value> = our_entries(&settings).collect();
    let claude = describe(Host::Claude, &ours, &Host::Claude.command(&exe), expected);
    let mut lines = vec![format!("claude: {claude} ({})", path.display())];
    lines.push(format!(
        "grok: {}",
        grok_status(&Host::Grok.command(&exe), expected).unwrap_or_else(|why| why)
    ));
    lines.extend(compat_status());
    lines.push(match trusted_exe() {
        Ok(path) => format!("executable: {path} (safe to register)"),
        Err(why) => format!("executable: {why}"),
    });
    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::{
        GROK_HOOK_TIMEOUT, Host, Verdict, entry_timeout, grok_handler, hook_timeout,
        installed_timeout, is_ours, record, run_budget, trusted_home, verdict,
    };
    use crate::config::{Meta, lstat, user};
    use serde_json::{Value, json};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    const COMMAND: &str = "'/bin/rotter' hook claude-stop";
    const GROK: &str = "'/bin/rotter' hook grok-stop || true";

    fn settings(timeouts: &[Value]) -> Value {
        let hooks: Vec<Value> = timeouts
            .iter()
            .map(|timeout| json!({ "type": "command", "command": COMMAND, "timeout": timeout }))
            .collect();
        json!({ "hooks": { "Stop": [{ "hooks": hooks }] } })
    }

    fn temp() -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "rotter-integration-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        fs::canonicalize(path).unwrap()
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

    #[test]
    fn grok_budget_uses_rotter_json_or_grok_default() {
        let document = |timeout: Value| {
            json!({ "hooks": { "Stop": [{ "hooks": [
                { "type": "command", "command": GROK, "timeout": timeout }
            ] }] } })
        };
        let grok = |document: &Value| {
            run_budget(
                hook_timeout(60),
                entry_timeout(document, GROK).unwrap_or(GROK_HOOK_TIMEOUT),
            )
        };
        assert_eq!(
            grok(&document(json!(10))),
            Duration::ZERO,
            "deadline = start"
        );
        assert_eq!(grok(&document(json!(90))), Duration::from_secs(75));
        assert_eq!(grok(&document(json!(0))), Duration::from_secs(75), "600");
        assert_eq!(grok(&json!({})), Duration::from_secs(75), "unreadable: 600");
        assert_eq!(
            run_budget(hook_timeout(3600), GROK_HOOK_TIMEOUT),
            Duration::from_secs(585)
        );
    }

    #[test]
    fn claude_ownership_accepts_both_command_forms() {
        let entry = |command: &str| json!({ "command": command });
        assert!(is_ours(&entry(COMMAND)));
        assert!(is_ours(&entry(&format!("{COMMAND} || true"))));
        assert!(!is_ours(&entry("'/bin/rotter' hook grok-stop || true")));
        assert!(!is_ours(&entry("'/bin/other' hook claude-stop")));
    }

    #[test]
    fn only_the_exact_rotter_document_is_managed() {
        let handler = json!({ "type": "command", "command": GROK, "timeout": 90 });
        let document = |handlers: Value| json!({ "hooks": { "Stop": [{ "hooks": handlers }] } });
        let good = document(json!([handler]));
        assert_eq!(grok_handler(&good), Some(&handler));
        let quoted = json!({ "type": "command", "timeout": 1,
            "command": r"'/opt/it'\''s/rotter' hook grok-stop || true" });
        assert!(grok_handler(&document(json!([quoted]))).is_some());
        let with = |key: &str, value: Value| {
            let mut handler = handler.clone();
            handler[key] = value;
            document(json!([handler]))
        };
        let rejected = [
            with("env", json!({})),
            with("timeout", json!(0)),
            with("timeout", json!("90")),
            with("type", json!("prompt")),
            with("command", json!("'/bin/rotter' hook grok-stop")),
            with("command", json!("'rotter' hook grok-stop || true")),
            with(
                "command",
                json!("'/bin/x' ; '/bin/y' hook grok-stop || true"),
            ),
            with("command", json!("/bin/rotter hook grok-stop || true")),
            document(json!([handler, handler])),
            json!({ "hooks": { "Stop": [{ "matcher": "", "hooks": [handler] }] } }),
            json!({ "hooks": { "Stop": [{ "hooks": [handler] }, { "hooks": [handler] }] } }),
            json!({ "hooks": { "Stop": [{ "hooks": [handler] }], "SessionStart": [] } }),
            json!({ "hooks": { "Stop": [{ "hooks": [handler] }] }, "x": 1 }),
            json!({ "x": 1 }),
            json!(null),
        ];
        for document in rejected {
            assert_eq!(grok_handler(&document), None, "{document}");
        }
    }

    /// The real lstat with owner and mode replaced for `at`.
    fn with(at: PathBuf, uid: u32, mode: u32) -> impl Fn(&Path) -> std::io::Result<Meta> {
        move |path| {
            let mut meta = lstat(path)?;
            if path == at {
                meta.uid = uid;
                meta.mode = mode;
            }
            Ok(meta)
        }
    }

    #[test]
    fn grok_home_must_be_the_users_private_directory() {
        let root = temp();
        let home = root.join("grok");
        assert_eq!(trusted_home(&home, user(), &lstat), Ok(None), "missing");
        fs::create_dir(&home).unwrap();
        assert_eq!(trusted_home(&home, user(), &lstat), Ok(Some(home.clone())));
        // resolve_trusted accepts a root-owned sticky directory; the Grok home does not.
        let sticky = with(home.clone(), 0, 0o41777);
        assert!(crate::config::resolve_trusted(&home, user(), &sticky).is_ok());
        // Root-owned: passes resolve_trusted, refused as not the user's.
        for mode in [0o41777, 0o40755] {
            let error = trusted_home(&home, user(), &with(home.clone(), 0, mode)).unwrap_err();
            assert!(error.contains("owned by you"), "{mode:o}: {error}");
        }
        for mode in [0o40770, 0o40702] {
            assert!(trusted_home(&home, user(), &with(home.clone(), user(), mode)).is_err());
        }
        fs::remove_dir_all(root).unwrap();
    }

    /// Replays Stops of (host, report, units) in one session; returns how many blocked.
    fn replay(turns: &[(Host, &str, usize)]) -> usize {
        let state = temp();
        let mut blocks = 0;
        for (host, report, found) in turns {
            match verdict(Some(&state), *host, "s", report, *found) {
                Verdict::Block(slot) => {
                    let (slot, fingerprint) = slot.unwrap();
                    record(&slot, &fingerprint);
                    blocks += 1;
                }
                Verdict::Quiet | Verdict::Unanalysed => {}
            }
        }
        fs::remove_dir_all(state).unwrap();
        blocks
    }

    #[test]
    fn double_registration_asks_once_per_unchanged_report() {
        use Host::{Claude, Grok};
        // Native Grok sees units; its Claude-compat twin times out with none. Three user turns
        // (continuations are skipped before any of this).
        let turn = [(Grok, "R", 3), (Claude, "Z", 0)];
        assert_eq!(replay(&turn.repeat(3)), 1);
        // Different budgets, different reports with units: each once.
        let turn = [(Grok, "R1", 3), (Claude, "R2", 2)];
        assert_eq!(replay(&turn.repeat(3)), 2);
        // Either order.
        assert_eq!(
            replay(&[(Claude, "R", 1), (Grok, "R", 1), (Claude, "R", 1)]),
            1
        );
        // One host alone, A→B→A: the other slot is empty, so A is requested again.
        assert_eq!(replay(&[(Grok, "A", 1), (Grok, "B", 1), (Grok, "A", 1)]), 3);
        // A→B→A is quiet when the other host's last block was A.
        assert_eq!(
            replay(&[
                (Claude, "A", 1),
                (Grok, "A", 1),
                (Grok, "B", 1),
                (Grok, "A", 1)
            ]),
            2
        );
        // No state directory: no dedupe.
        assert_eq!(verdict(None, Grok, "s", "R", 1), Verdict::Block(None));
    }
}
