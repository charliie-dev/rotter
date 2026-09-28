use crate::config::{
    self, Config, Kind, Meta, Sources, Untrusted, lstat, resolve_trusted, trusted_file, user,
};
use crate::git::{self, Git, Scratch};
use crate::grammar::{O_NOFOLLOW, create_private, open_regular};
use crate::hosts::{self, CLAUDE, Host, Install, Notes};
use crate::json::Json;
use crate::{Mode, Options, extract_with, toplevel};
use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Value, json};
use std::fmt;
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The smallest hook timeout rotter installs (Claude Code's own default).
const MIN_HOOK_TIMEOUT: u64 = 60;
/// Seconds of the hook timeout kept free for git and reporting after the per-file phases.
const HOOK_RESERVE: u64 = 15;
/// Most consecutive review requests per host and session; the next Stop that would ask is silent
/// instead, and that silent Stop resets the count.
const MAX_REQUESTS: u32 = 2;

/// `'<exe>' hook <name> || true` with the host's canonical hook name: a missing or older binary
/// can never exit 2 and so never forces continuations.
fn command(host: &Host, exe: &str) -> String {
    format!("{} hook {} || true", shell_quote(exe), host.hook)
}

/// The host's directory: its variable when absolute, else `<home>/<fallback>`.
fn host_dir(host: &Host, sources: &Sources) -> Result<PathBuf, String> {
    host.dir_var
        .and_then(|name| sources.var(name))
        .map(Path::to_owned)
        .or_else(|| sources.home.as_ref().map(|home| home.join(host.fallback)))
        .ok_or_else(|| match host.dir_var {
            Some(name) => format!(
                "cannot locate {}'s directory: set HOME or an absolute {name}",
                host.label
            ),
            None => format!("cannot locate {}'s directory: set HOME", host.label),
        })
}

/// `(nested, grouped)`: whether the event map is the document's `"hooks"` object, and whether
/// the event's array holds `{"hooks":[…]}` groups rather than handlers.
fn shape(host: &Host) -> (bool, bool) {
    match host.install {
        Install::MergeJson { nested, .. } => (nested, true),
        Install::OwnedJson { grouped, .. } => (true, grouped),
    }
}

/// The value at the host's event in a hook document (null when absent).
fn event_list<'a>(host: &Host, document: &'a Value) -> &'a Value {
    if shape(host).0 {
        &document["hooks"][host.event]
    } else {
        &document[host.event]
    }
}

/// The file holding the host's hook under `dir`.
fn hook_file(host: &Host, dir: &Path) -> PathBuf {
    match host.install {
        Install::MergeJson { file, .. } => dir.join(file),
        Install::OwnedJson { dir: sub, file, .. } => dir.join(sub).join(file),
    }
}

/// The hook `timeout` to install: room for one full parse plus git and reporting.
pub fn hook_timeout(parse_timeout_seconds: u64) -> u64 {
    MIN_HOOK_TIMEOUT.max(parse_timeout_seconds.saturating_add(30))
}

/// Smallest positive integer `timeout` among the host's event entries with exactly this command.
fn entry_timeout(host: &Host, settings: &Value, command: &str) -> Option<u64> {
    event_entries(host, settings)
        .into_iter()
        .filter(|entry| entry[host.command_key].as_str() == Some(command))
        .filter_map(|entry| {
            entry[host.timeout_key]
                .as_u64()
                .filter(|timeout| *timeout > 0)
        })
        .min()
}

/// Soft budget for the per-file phases of one hook run.
fn run_budget(computed: u64, installed: u64) -> Duration {
    Duration::from_secs(computed.min(installed).saturating_sub(HOOK_RESERVE))
}

/// The installed timeout from the host's hook file, read without following a final link or
/// blocking on a FIFO; anything unusable counts as the host's own default.
fn installed_timeout(host: &Host, sources: &Sources, command: &str) -> u64 {
    host_dir(host, sources)
        .ok()
        .and_then(|dir| read_settings(&hook_file(host, &dir)).ok())
        .and_then(|(settings, _)| entry_timeout(host, &settings, command))
        .unwrap_or(host.default_timeout)
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

/// The session id from the host input, mapped to a file name (`[A-Za-z0-9._-]`, others `_`).
/// A missing, empty, `.`, `..` or over-255-byte id is None: neither the loop cap nor dedupe
/// can be kept, so the Stop is silent.
fn session(host: &Host, input: &Value) -> Option<String> {
    let raw = host
        .session_keys
        .iter()
        .find_map(|key| input[key].as_str())?;
    if raw.is_empty() || raw == "." || raw == ".." || raw.len() > 255 {
        return None;
    }
    Some(
        raw.chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() || "._-".contains(character) {
                    character
                } else {
                    '_'
                }
            })
            .collect(),
    )
}

/// The first present cwd key, used only when absolute and free of control characters; without
/// one, the hook process's own directory for hosts that run the hook there.
fn cwd(host: &Host, input: &Value) -> Option<PathBuf> {
    match host.cwd_keys.iter().find_map(|key| input[key].as_str()) {
        Some(cwd) => {
            (cwd.starts_with('/') && !cwd.contains(char::is_control)).then(|| PathBuf::from(cwd))
        }
        None if host.cwd_fallback => std::env::current_dir().ok(),
        None => None,
    }
}

/// A Stop hook run for `host` with every directory and PATH from `sources`. A block goes out as
/// `{"decision":"block","reason"}`; messages as a `systemMessage` or on stderr, per host.
/// Never fails.
pub fn stop(host: &Host, input: &str, sources: &Sources) {
    let input: Value = serde_json::from_str(input).unwrap_or(Value::Null);
    let Some(session) = session(host, &input) else {
        return;
    };
    let mut outcome = evaluate(host, &input, &session, sources).unwrap_or_default();
    // The loop cap fails closed: a request goes out only once it has been counted.
    let state = sources.state.as_deref();
    if outcome.reason.is_some() {
        if !state.is_some_and(|state| allow_request(state, host, &session)) {
            outcome.reason = None;
            outcome.record = None;
        }
    } else if let Some(state) = state {
        reset_requests(state, host, &session);
    }
    let messages =
        (!outcome.messages.is_empty()).then(|| format!("rotter: {}", outcome.messages.join("; ")));
    let output = match (outcome.reason, host.notes) {
        (Some(reason), notes) => {
            let mut output = json!({ "decision": "block", "reason": reason });
            if notes == Notes::SystemMessage
                && let Some(messages) = &messages
            {
                output["systemMessage"] = messages.clone().into();
            }
            Some(output)
        }
        (None, Notes::SystemMessage) => messages
            .as_ref()
            .map(|messages| json!({ "systemMessage": messages })),
        (None, Notes::Stderr) => None,
    };
    if host.notes == Notes::Stderr
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
fn evaluate(host: &Host, input: &Value, session: &str, sources: &Sources) -> Option<Outcome> {
    let start = Instant::now();
    if host
        .continuation_keys
        .iter()
        .any(|key| input[key] == Value::Bool(true))
    {
        return None;
    }
    if let Some((key, value)) = host.end_reason
        && input.get(key).is_some_and(|reason| reason != value)
    {
        return None;
    }
    let state = sources.state.as_deref();
    // Messages on a channel the host shows are announced once per session; stderr notes are not
    // shown, so they are never marked as announced and recur, harmlessly.
    let announce = |store: &str, text: &str| {
        host.notes == Notes::Stderr
            || state.is_none_or(|dir| {
                first_time(&dir.join(format!("{}-{store}", host.hook)), session, text)
            })
    };
    let cwd = cwd(host, input)?;
    // No git runs before the physical cwd is known, and none at all without a `.git` above it.
    let walk = git::walk(&cwd).ok()?;
    walk.repository.as_ref()?;
    let mut outcome = Outcome::default();
    // Persistent failures (e.g. a refused TMPDIR) would otherwise repeat on every Stop.
    let scratch = match Scratch::new(&sources.temp) {
        Ok(scratch) => scratch,
        Err(error) => {
            let text = format!("extract failed: {error}");
            if announce("errors", &text) {
                outcome.messages.push(text);
            }
            return Some(outcome);
        }
    };
    // Without a usable git (native, trusted, outside the repository) there is nothing to do.
    let git = Git::new(&walk, scratch, sources, true).ok()?;
    // Too old or unrecognised git: no repository is looked at, not even to find its top level.
    if let Err(error) = git.class() {
        if announce("errors", &error) {
            outcome.messages.push(error);
        }
        return Some(outcome);
    }
    // Outside a work tree, or a work tree holding the chosen git: nothing to do.
    let top = toplevel(&git, &cwd).ok()?;
    // Config problems never block: they are announced and builtins are used.
    let mut notes = Vec::new();
    let config = match config::load(Some(&top), sources) {
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
    let run = format!(
        "{rotter} extract --worktree --include-untracked -C {}",
        shell_quote(&cwd.display().to_string())
    );
    let installed = installed_timeout(host, sources, &command(host, &exe()));
    let budget = run_budget(hook_timeout(config.parse_timeout_seconds), installed);
    let mut options = Options::new(Mode::Worktree);
    options.include_untracked = true;
    options.grammars = config.languages();
    options.languages = config.override_languages(&options.grammars);
    options.parse_timeout = config.parse_timeout();
    options.deadline = start.checked_add(budget);
    let report = match extract_with(&git, &cwd, &options) {
        Ok(report) => report,
        Err(error) => {
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
    match verdict(state, host, session, &key, found) {
        Verdict::Quiet => {}
        Verdict::Unanalysed => {
            if announce("errors", &key) {
                outcome.messages.push(format!(
                    "some changed files could not be analysed; run {run}"
                ));
            }
        }
        Verdict::Block(record) => {
            outcome.reason = Some(format!(
                "rotter found {found} changed code unit(s) with related comments in {} (report complete: {}). \
                 Before finishing, review them with the rotter-comment-review skill in working tree mode \
                 including untracked files: run `{run}`. If that skill is not loaded, run \
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

/// `<state>/<hook>-count/<session>`, the host+session request count, opened under the resolved
/// state directory without following links and locked with flock(2) (released when the
/// descriptor closes, also if rotter dies). The count is updated in place on this descriptor,
/// never renamed or unlinked, so the lock and the count share one inode. With `create`, missing
/// directories are made 0700 one at a time below a trusted ancestor and a missing file is
/// created; without, a missing one is None (nothing to reset).
fn counter(
    state: &Path,
    host: &Host,
    session: &str,
    create: bool,
) -> Result<Option<fs::File>, String> {
    if create {
        crate::install::create_base(state)?;
    }
    let state = match resolve_trusted(state, user(), &lstat) {
        Ok(resolved) => resolved,
        Err(Untrusted::Missing) if !create => return Ok(None),
        Err(Untrusted::Missing) => return Err(format!("{} vanished", state.display())),
        Err(Untrusted::Refused(why)) => return Err(why),
    };
    let dir = state.join(format!("{}-count", host.hook));
    let fail = |error: io::Error| format!("{}: {error}", dir.display());
    match lstat(&dir) {
        Ok(meta) => owned_dir(&dir, meta, user())?,
        Err(error) if error.kind() == io::ErrorKind::NotFound && create => {
            match fs::DirBuilder::new().mode(0o700).create(&dir) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(fail(error)),
            }
            owned_dir(&dir, lstat(&dir).map_err(fail)?, user())?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(fail(error)),
    }
    let path = dir.join(session);
    let fail = |error: io::Error| format!("{}: {error}", path.display());
    let file = match fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .mode(0o600)
        .custom_flags(O_NOFOLLOW)
        .open(&path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound && !create => return Ok(None),
        Err(error) => return Err(fail(error)),
    };
    let meta = file.metadata().map_err(fail)?;
    if !meta.is_file() || meta.uid() != user() || meta.mode() & 0o022 != 0 {
        return Err(format!(
            "{} must be a regular file owned by you that others cannot write",
            path.display()
        ));
    }
    file.lock().map_err(fail)?;
    Ok(Some(file))
}

/// The count in a locked counter file: empty is 0, else digits and a newline; None when it
/// cannot be read or is anything else.
fn read_count(file: &mut fs::File) -> Option<u32> {
    let mut text = String::new();
    file.read_to_string(&mut text).ok()?;
    if text.is_empty() {
        return Some(0);
    }
    let digits = text.strip_suffix('\n')?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// Writes `count` in place: truncate, then the digits (0 is the empty file).
fn write_count(file: &fs::File, count: u32) -> io::Result<()> {
    file.set_len(0)?;
    if count > 0 {
        file.write_all_at(format!("{count}\n").as_bytes(), 0)?;
    }
    Ok(())
}

/// Counts one review request for `host` and `session`. False (the Stop stays silent) when the
/// cap is reached, which resets the count, or when the count cannot be read or written.
fn allow_request(state: &Path, host: &Host, session: &str) -> bool {
    let Ok(Some(mut file)) = counter(state, host, session, true) else {
        return false;
    };
    let Some(count) = read_count(&mut file) else {
        return false;
    };
    if count >= MAX_REQUESTS {
        let _ = write_count(&file, 0);
        return false;
    }
    write_count(&file, count + 1).is_ok()
}

/// A Stop that asks nothing ends the host's chain: the count goes back to 0.
fn reset_requests(state: &Path, host: &Host, session: &str) {
    if let Ok(Some(file)) = counter(state, host, session, false) {
        let _ = write_count(&file, 0);
    }
}

#[derive(Debug, PartialEq)]
enum Verdict {
    /// Every host's slot is checked: this report was the last block of some host.
    Quiet,
    /// No units, only an incomplete report: a diagnostic, which never touches the slots.
    Unanalysed,
    /// A review request and the emitting host's slot to record after it is printed (None
    /// without a state directory; the loop cap then keeps it from going out).
    Block(Option<(PathBuf, String)>),
}

/// What a Stop does with a changed report `key` holding `found` units. Each host keeps one "last
/// blocked" fingerprint per session; a report equal to any host's is quiet, so a native Grok
/// hook and its Claude-compat twin ask once for the same report, while a report that changes and
/// later returns may be requested again. Concurrent runs may both ask (accepted).
fn verdict(state: Option<&Path>, host: &Host, session: &str, key: &str, found: usize) -> Verdict {
    if found == 0 {
        return Verdict::Unanalysed;
    }
    let Some(state) = state else {
        return Verdict::Block(None);
    };
    let fingerprint = fingerprint(key);
    if hosts::HOSTS
        .iter()
        .any(|other| seen(&slot(state, other, session)).as_deref() == Some(fingerprint.as_str()))
    {
        return Verdict::Quiet;
    }
    Verdict::Block(Some((slot(state, host, session), fingerprint)))
}

/// `<state>/<hook>/<session>`: the host's last blocked fingerprint.
fn slot(state: &Path, host: &Host, session: &str) -> PathBuf {
    state.join(host.hook).join(session)
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

/// How a hook entry relates to rotter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Ours {
    /// `'<this binary>' hook <name> || true`.
    Current,
    /// This binary without ` || true` (the pre-S0 Claude form): a mismatch install rewrites.
    Legacy,
    /// The same shape for another absolute path ending in `/rotter`: another rotter binary,
    /// replaced by install.
    Other,
}

/// Parses exactly `'<path>' hook <name>[ || true]` with the host's canonical name, un-quoting
/// the `'\''` form rotter writes; anything else (e.g. `'/x/rotter-proxy' hook codex`) is foreign
/// and never touched.
fn ours(host: &Host, command: &str, exe: &str) -> Option<Ours> {
    let (rest, current) = match command.strip_suffix(" || true") {
        Some(rest) => (rest, true),
        None => (command, false),
    };
    let quoted = rest.strip_suffix(format!(" hook {}", host.hook).as_str())?;
    let path = quoted
        .strip_prefix('\'')?
        .strip_suffix('\'')?
        .replace(r"'\''", "'");
    if !path.starts_with('/') || shell_quote(&path) != quoted || !(current || host.legacy) {
        return None;
    }
    match (path == exe, current) {
        (true, true) => Some(Ours::Current),
        (true, false) => Some(Ours::Legacy),
        (false, _) if path.ends_with("/rotter") => Some(Ours::Other),
        (false, _) => None,
    }
}

/// A JSON value parsed with duplicate object keys refused (serde_json keeps the last one).
struct Strict(Value);

impl<'de> Deserialize<'de> for Strict {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(StrictVisitor).map(Strict)
    }
}

struct StrictVisitor;

impl<'de> Visitor<'de> for StrictVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Value, E> {
        Ok(value.into())
    }

    fn visit_i64<E>(self, value: i64) -> Result<Value, E> {
        Ok(value.into())
    }

    fn visit_u64<E>(self, value: u64) -> Result<Value, E> {
        Ok(value.into())
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Value, E> {
        serde_json::Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| E::custom("not a JSON number"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Value, E> {
        Ok(value.into())
    }

    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut items = Vec::new();
        while let Some(Strict(item)) = seq.next_element()? {
            items.push(item);
        }
        Ok(Value::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut object = serde_json::Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if object.contains_key(&key) {
                return Err(de::Error::custom(format!("duplicate key {key:?}")));
            }
            let Strict(value) = map.next_value()?;
            object.insert(key, value);
        }
        Ok(Value::Object(object))
    }
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
    Ok((object(path, &text)?, Some(text)))
}

/// `text` parsed as a JSON object.
fn object(path: &Path, text: &str) -> Result<Value, String> {
    let value: Value = serde_json::from_str(text)
        .map_err(|error| format!("cannot parse {}: {error}", path.display()))?;
    if value.is_object() {
        Ok(value)
    } else {
        Err(format!("{} is not a JSON object", path.display()))
    }
}

/// A file rotter may rewrite: owned by the user and not writable by group or others.
fn rewritable(path: &Path, uid: u32, mode: u32, user: u32) -> Result<(), String> {
    if uid != user {
        return Err(format!(
            "{} is not owned by you; left alone",
            path.display()
        ));
    }
    if mode & 0o022 != 0 {
        return Err(format!(
            "{} is writable by group or others; left alone. Run `chmod go-w {}` and try again",
            path.display(),
            shell_quote(&path.display().to_string())
        ));
    }
    Ok(())
}

/// A shared JSON file rotter merges into, as [`read_settings`] reads it, when it is
/// [`rewritable`] and rewriting it changes nothing but rotter's entry: its re-serialised form
/// (serde_json keeps key order but collapses duplicate keys and normalises numbers) must equal,
/// as JSON values, a strict parse that refuses duplicate keys.
fn read_merged(path: &Path) -> Result<(Value, Option<String>), String> {
    let Some(mut file) = open_regular(path)? else {
        return Ok((json!({}), None));
    };
    let fail = |error: io::Error| format!("cannot read {}: {error}", path.display());
    let meta = file.metadata().map_err(fail)?;
    rewritable(path, meta.uid(), meta.mode(), user())?;
    let mut text = String::new();
    file.read_to_string(&mut text).map_err(fail)?;
    let value = object(path, &text)?;
    faithful(path, &value, &text)?;
    Ok((value, Some(text)))
}

/// Whether re-serialising `value` (parsed from `text`) keeps every other tool's content.
fn faithful(path: &Path, value: &Value, text: &str) -> Result<(), String> {
    let Strict(strict) = serde_json::from_str(text)
        .map_err(|error| format!("{}: {error}; left alone", path.display()))?;
    let rewritten = serde_json::to_string(value).map_err(|error| error.to_string())?;
    if serde_json::from_str::<Value>(&rewritten).ok().as_ref() != Some(&strict) {
        return Err(format!(
            "{} would change when rotter rewrites it; left alone",
            path.display()
        ));
    }
    Ok(())
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
/// without following symlinks and gets `mode` through its descriptor before the rename. With
/// `before` (the bytes read earlier, None for a missing file), `path` is re-read just before the
/// rename and the write is abandoned when it changed meanwhile; a change after that re-read is
/// still lost (a small window).
fn replace_file(
    path: &Path,
    text: &str,
    mode: Option<u32>,
    before: Option<Option<&[u8]>>,
) -> Result<(), String> {
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
    if let Some(before) = before {
        let now = match open_regular(path) {
            Ok(Some(mut file)) => {
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes).map(|_| Some(bytes))
            }
            Ok(None) => Ok(None),
            Err(why) => Err(io::Error::other(why)),
        };
        if now.as_ref().ok().map(Option::as_deref) != Some(before) {
            let _ = fs::remove_file(&temporary);
            return Err(format!(
                "{} changed while rotter was updating it; nothing was written, run the command \
                 again",
                path.display()
            ));
        }
    }
    fs::rename(&temporary, path)
        .map_err(|error| format!("cannot replace {}: {error}", path.display()))
}

/// Writes a merged file atomically, keeping the original's permission bits, unless it changed
/// since `original` was read.
fn write_merged(path: &Path, value: &Value, original: Option<&str>) -> Result<(), String> {
    let text = serde_json::to_string_pretty(value).map_err(|error| error.to_string())? + "\n";
    let mode = fs::symlink_metadata(path)
        .ok()
        .filter(fs::Metadata::is_file)
        .map(|meta| meta.permissions().mode() & 0o777);
    replace_file(path, &text, mode, Some(original.map(str::as_bytes)))
}

/// The groups of the host's event in a merged file, when it is an array.
fn event_groups_mut<'a>(host: &Host, settings: &'a mut Value) -> Option<&'a mut Vec<Value>> {
    // get_mut, not IndexMut: indexing would insert `"hooks": null` into untouched settings.
    let events = if shape(host).0 {
        settings.get_mut("hooks")?
    } else {
        settings
    };
    events.get_mut(host.event)?.as_array_mut()
}

/// Removes rotter's entries (any [`Ours`]) and returns how many were removed. Only groups this
/// emptied are removed; other tools' empty groups stay.
fn remove_ours(host: &Host, settings: &mut Value, exe: &str) -> usize {
    prune_ours(host, settings, exe, |_| false)
}

/// Visits rotter's entries in file order; `keep` may rewrite one in place and keep it (true)
/// or have it removed (false). Returns how many rotter entries there were. Only groups this
/// emptied are removed, so other groups keep their positions (Codex keys hook trust on them).
fn prune_ours(
    host: &Host,
    settings: &mut Value,
    exe: &str,
    mut keep: impl FnMut(&mut Value) -> bool,
) -> usize {
    let Some(groups) = event_groups_mut(host, settings) else {
        return 0;
    };
    let mut found = 0;
    let mut emptied = Vec::new();
    for (index, group) in groups.iter_mut().enumerate() {
        if let Some(hooks) = group.get_mut("hooks").and_then(Value::as_array_mut) {
            let before = hooks.len();
            hooks.retain_mut(|entry| {
                if entry_ours(host, entry, exe).is_none() {
                    return true;
                }
                found += 1;
                keep(entry)
            });
            if hooks.is_empty() && before > 0 {
                emptied.push(index);
            }
        }
    }
    let mut index = 0;
    groups.retain(|_| {
        index += 1;
        !emptied.contains(&(index - 1))
    });
    found
}

fn entry_ours(host: &Host, entry: &Value, exe: &str) -> Option<Ours> {
    entry[host.command_key]
        .as_str()
        .and_then(|command| ours(host, command, exe))
}

fn target(name: &str) -> Result<&'static Host, String> {
    if let Some((_, why)) = hosts::UNSUPPORTED.iter().find(|(id, _)| *id == name) {
        return Err(format!("{name} is not supported: {why}"));
    }
    hosts::by_id(name).ok_or_else(|| {
        format!(
            "unknown integration {name:?}; available: {}",
            hosts::HOSTS
                .iter()
                .map(|host| host.id)
                .collect::<Vec<_>>()
                .join(", ")
        )
    })
}

/// Every hook entry of the host's event, in any group.
fn event_entries<'a>(host: &Host, settings: &'a Value) -> Vec<&'a Value> {
    let items = event_list(host, settings).as_array().into_iter().flatten();
    if shape(host).1 {
        items
            .flat_map(|group| group["hooks"].as_array().into_iter().flatten())
            .collect()
    } else {
        items.collect()
    }
}

/// The host's entries that belong to some rotter binary, with how.
fn our_entries<'a>(host: &Host, settings: &'a Value, exe: &str) -> Vec<(Ours, &'a Value)> {
    event_entries(host, settings)
        .into_iter()
        .filter_map(|entry| Some((entry_ours(host, entry, exe)?, entry)))
        .collect()
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

/// The host directory `home` through `resolve_trusted`, then [`owned_dir`]; None when it does
/// not exist.
fn trusted_home(
    host: &Host,
    home: &Path,
    user: u32,
    lstat: &dyn Fn(&Path) -> io::Result<Meta>,
) -> Result<Option<PathBuf>, String> {
    let resolved = match resolve_trusted(home, user, lstat) {
        Ok(resolved) => resolved,
        Err(Untrusted::Missing) => return Ok(None),
        Err(Untrusted::Refused(why)) => {
            return Err(format!(
                "{}'s directory {} refused: {why}",
                host.label,
                home.display()
            ));
        }
    };
    let meta = lstat(&resolved).map_err(|error| format!("{}: {error}", resolved.display()))?;
    owned_dir(&resolved, meta, user)?;
    Ok(Some(resolved))
}

/// [`trusted_home`] that must exist, for install.
fn existing_home(host: &Host, home: &Path) -> Result<PathBuf, String> {
    trusted_home(host, home, user(), &lstat)?.ok_or_else(|| {
        format!(
            "{} does not exist; start {} once or create it",
            home.display(),
            host.label
        )
    })
}

/// Refuses a host directory inside a git work tree: a checkout, pull or commit there could
/// change or publish the hook, and ignoring the file does not take it out of the work tree. The
/// physical `.git` walk runs first and needs no git; only with a `.git` above does git run,
/// resolved like every other git call, isolated (cleared environment, no inherited GIT_*, a
/// neutral cwd), and only a clean "not a git repository" counts as outside.
fn outside_work_tree(host: &Host, dir: &Path, sources: &Sources) -> Result<(), String> {
    let refuse = |why: String| {
        format!(
            "refusing to install into {}: cannot tell whether it is inside a git work tree: {why}",
            dir.display()
        )
    };
    let walk = git::walk(dir).map_err(refuse)?;
    if walk.repository.is_none() {
        return Ok(());
    }
    let scratch = Scratch::new(&sources.temp).map_err(refuse)?;
    let git = Git::new(&walk, scratch, sources, true).map_err(refuse)?;
    match git.work_tree(dir).map_err(refuse)? {
        None => Ok(()),
        Some(top) => Err(format!(
            "refusing to install into {}: it is inside the git work tree {}, where a checkout or \
             commit could change or publish the hook; ignoring the file does not change that. \
             Move {}'s directory outside the work tree or point {} at one outside it",
            dir.display(),
            top.display(),
            host.label,
            host.dir_var.unwrap_or("HOME")
        )),
    }
}

/// `<resolved home>/<dir>/<file>`, with `<dir>` checked by [`owned_dir`] (lstat, so never a
/// symlink); None when `<dir>` does not exist. With `create`, a missing `<dir>` is created at
/// 0700 (never its parents).
fn owned_file(host: &Host, home: &Path, create: bool) -> Result<Option<PathBuf>, String> {
    let Install::OwnedJson { dir, file, .. } = host.install else {
        unreachable!("an owned-file host");
    };
    let hooks = home.join(dir);
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
    Ok(Some(hooks.join(file)))
}

/// The document rotter writes for an owned-file host: see [`owned_handler`].
fn owned_document(host: &Host, command: &str, timeout: u64) -> Value {
    let Install::OwnedJson {
        version, grouped, ..
    } = host.install
    else {
        unreachable!("an owned-file host");
    };
    let mut handler = serde_json::Map::new();
    handler.insert("type".into(), "command".into());
    handler.insert(host.command_key.into(), command.into());
    handler.insert(host.timeout_key.into(), timeout.into());
    let item = if grouped {
        json!({ "hooks": [handler] })
    } else {
        Value::Object(handler)
    };
    let mut document = serde_json::Map::new();
    if let Some(version) = version {
        document.insert("version".into(), version.into());
    }
    document.insert("hooks".into(), json!({ host.event: [item] }));
    Value::Object(document)
}

/// The only handler of a rotter-generated document, which parses to exactly
/// `{["version":V,]"hooks":{"<event>":[H]}}` (H wrapped as `{"hooks":[H]}` for grouped hosts,
/// V only for hosts that version the file) with H `{"type":"command",<command key>:C,<timeout
/// key>:N}`, C `'<absolute path>' hook <name> || true` and N a positive integer; None for
/// anything else.
fn owned_handler<'a>(host: &Host, document: &'a Value) -> Option<&'a Value> {
    fn only<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
        let object = value.as_object()?;
        if object.len() == 1 {
            object.get(key)
        } else {
            None
        }
    }
    let Install::OwnedJson {
        version, grouped, ..
    } = host.install
    else {
        return None;
    };
    let top = document.as_object()?;
    let hooks = match version {
        Some(version) if top.len() == 2 && top.get("version")?.as_u64() == Some(version) => {
            top.get("hooks")?
        }
        Some(_) => return None,
        None => only(document, "hooks")?,
    };
    let [item] = only(hooks, host.event)?.as_array()?.as_slice() else {
        return None;
    };
    let handler = if grouped {
        let [handler] = only(item, "hooks")?.as_array()?.as_slice() else {
            return None;
        };
        handler
    } else {
        item
    };
    let fields = handler.as_object()?;
    let quoted = fields
        .get(host.command_key)?
        .as_str()?
        .strip_suffix(format!(" hook {} || true", host.hook).as_str())?;
    let path = quoted
        .strip_prefix('\'')?
        .strip_suffix('\'')?
        .replace(r"'\''", "'");
    (fields.len() == 3
        && fields.get("type")? == "command"
        && fields
            .get(host.timeout_key)?
            .as_u64()
            .is_some_and(|timeout| timeout > 0)
        && path.starts_with('/')
        && shell_quote(&path) == quoted)
        .then_some(handler)
}

/// The handler of an installed owned file; None when there is none. It must be a regular file
/// (never followed, no FIFO block) owned by the user that group and others cannot write, holding
/// a rotter-generated document; anything else is an error and the file is left alone.
fn owned_installed(host: &Host, path: &Path) -> Result<Option<Value>, String> {
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
        .and_then(|document| owned_handler(host, document))
        .map(|handler| Some(handler.clone()))
        .ok_or_else(|| format!("{} is not managed by rotter; left alone", path.display()))
}

/// Installs the host's hook with `timeout` sized for `parse_timeout_seconds`.
pub fn install(name: &str, parse_timeout_seconds: u64) -> Result<String, String> {
    let host = target(name)?;
    let sources = Sources::from_env();
    // Checked before anything is written: the host runs this path on every Stop.
    let exe = trusted_exe()?;
    let timeout = hook_timeout(parse_timeout_seconds);
    match host.install {
        Install::MergeJson { .. } => install_merged(host, &sources, &exe, timeout),
        Install::OwnedJson { .. } => install_owned(host, &sources, &exe, timeout),
    }
}

/// Merges one entry into the host's shared file, keeping a `.rotter-bak` of the bytes read.
fn install_merged(
    host: &Host,
    sources: &Sources,
    exe: &str,
    timeout: u64,
) -> Result<String, String> {
    let configured = host_dir(host, sources)?;
    let display = hook_file(host, &configured);
    let dir = existing_home(host, &configured)?;
    outside_work_tree(host, &dir, sources)?;
    let path = hook_file(host, &dir);
    let (mut settings, original) = read_merged(&path)?;
    if original.is_none() {
        unshadowed(host, &dir)?;
    }
    let command = command(host, exe);
    // Current only when it is the one entry and both command and timeout match; otherwise
    // (the older form, another rotter binary, several entries) it is replaced.
    let current = our_entries(host, &settings, exe)
        .iter()
        .map(|(how, entry)| {
            *how == Ours::Current && entry[host.timeout_key].as_u64() == Some(timeout)
        })
        .collect::<Vec<_>>();
    if current == [true] {
        return Ok(format!(
            "{}: already installed ({})",
            host.id,
            display.display()
        ));
    }
    let mut entry = serde_json::Map::new();
    entry.insert("type".into(), "command".into());
    entry.insert(host.command_key.into(), command.into());
    entry.insert(host.timeout_key.into(), timeout.into());
    let entry = Value::Object(entry);
    // The first rotter entry is rewritten where it is and any others are removed, so no other
    // hook changes position.
    let mut placed = false;
    let replaced = prune_ours(host, &mut settings, exe, |slot| {
        if placed {
            return false;
        }
        slot.clone_from(&entry);
        placed = true;
        true
    });
    if !placed {
        let events = if shape(host).0 {
            if !settings["hooks"].is_object() {
                settings["hooks"] = json!({});
            }
            &mut settings["hooks"]
        } else {
            &mut settings
        };
        if !events[host.event].is_array() {
            events[host.event] = json!([]);
        }
        events[host.event]
            .as_array_mut()
            .expect("the event is an array")
            .push(json!({ "hooks": [entry] }));
    }
    if let Some(original) = &original {
        write_backup(&path, original)?;
    }
    write_merged(&path, &settings, original.as_deref())?;
    let verb = if replaced > 0 { "updated" } else { "installed" };
    Ok(format!(
        "{}: {verb} {} hook in {}",
        host.id,
        host.event,
        display.display()
    ))
}

/// Refuses to create the host's hook file while a file it shadows declares hooks: the host
/// reads those only while the hook file is missing, so creating it would disable them.
fn unshadowed(host: &Host, dir: &Path) -> Result<(), String> {
    for name in host.shadows {
        let path = dir.join(name);
        let (settings, _) = read_settings(&path).map_err(|why| {
            format!(
                "cannot tell whether {} declares hooks: {why}",
                path.display()
            )
        })?;
        let declares = match &settings["hooks"] {
            Value::Null => false,
            Value::Object(events) => !events.is_empty(),
            Value::Array(items) => !items.is_empty(),
            _ => true,
        };
        if declares {
            return Err(format!(
                "refusing to create {}: {} declares hooks, which {} reads only while that file is \
                 missing. Move them into it first (Droid's /hooks does this on its next save)",
                hook_file(host, dir).display(),
                path.display(),
                host.label
            ));
        }
    }
    Ok(())
}

/// Writes the host's owned file, which rotter owns entirely (no backup).
fn install_owned(
    host: &Host,
    sources: &Sources,
    exe: &str,
    timeout: u64,
) -> Result<String, String> {
    let home = existing_home(host, &host_dir(host, sources)?)?;
    outside_work_tree(host, &home, sources)?;
    let path = owned_file(host, &home, true)?.ok_or("the hooks directory vanished")?;
    let installed = owned_installed(host, &path)?;
    let command = command(host, exe);
    if let Some(handler) = &installed
        && handler[host.command_key] == command
        && handler[host.timeout_key] == timeout
    {
        return Ok(format!(
            "{}: already installed ({})",
            host.id,
            path.display()
        ));
    }
    let document = owned_document(host, &command, timeout);
    let text = serde_json::to_string_pretty(&document).map_err(|error| error.to_string())? + "\n";
    // Exactly 0600 whatever the umask; nothing is copied from the old file.
    replace_file(&path, &text, Some(0o600), None)?;
    let verb = if installed.is_some() {
        "updated"
    } else {
        "installed"
    };
    Ok(format!(
        "{}: {verb} {} hook in {}",
        host.id,
        host.event,
        path.display()
    ))
}

pub fn uninstall(name: &str) -> Result<String, String> {
    let host = target(name)?;
    let sources = Sources::from_env();
    match host.install {
        Install::MergeJson { .. } => uninstall_merged(host, &sources),
        Install::OwnedJson { .. } => uninstall_owned(host, &sources),
    }
}

fn uninstall_merged(host: &Host, sources: &Sources) -> Result<String, String> {
    let configured = host_dir(host, sources)?;
    let display = hook_file(host, &configured);
    let Some(dir) = trusted_home(host, &configured, user(), &lstat)? else {
        return Ok(format!(
            "{}: not installed ({})",
            host.id,
            display.display()
        ));
    };
    let path = hook_file(host, &dir);
    // An unreadable or unparseable file stops here and keeps any backup.
    let (mut settings, original) = read_merged(&path)?;
    let mut lines = vec![if remove_ours(host, &mut settings, &exe()) == 0 {
        format!("{}: not installed ({})", host.id, display.display())
    } else {
        write_merged(&path, &settings, original.as_deref())?;
        format!(
            "{}: removed {} hook from {}",
            host.id,
            host.event,
            display.display()
        )
    }];
    // unlink never follows a symlink, and only a regular file is removed at all.
    let backup = backup_path(&path);
    let shown = backup_path(&display);
    match fs::symlink_metadata(&backup) {
        Ok(meta) if meta.is_file() => {
            fs::remove_file(&backup)
                .map_err(|error| format!("cannot remove {}: {error}", shown.display()))?;
            lines.push(format!("{}: removed {}", host.id, shown.display()));
        }
        Ok(_) => lines.push(format!(
            "{}: left {} alone: it is not a regular file",
            host.id,
            shown.display()
        )),
        Err(_) => {}
    }
    Ok(lines.join("\n"))
}

/// Unlinks the owned file only when [`owned_installed`] accepts it.
fn uninstall_owned(host: &Host, sources: &Sources) -> Result<String, String> {
    let configured = host_dir(host, sources)?;
    let path = match trusted_home(host, &configured, user(), &lstat)? {
        Some(home) => owned_file(host, &home, false)?,
        None => None,
    };
    let Some(path) = path else {
        return Ok(format!(
            "{}: not installed ({})",
            host.id,
            hook_file(host, &configured).display()
        ));
    };
    if owned_installed(host, &path)?.is_none() {
        return Ok(format!("{}: not installed ({})", host.id, path.display()));
    }
    fs::remove_file(&path).map_err(|error| format!("cannot remove {}: {error}", path.display()))?;
    Ok(format!(
        "{}: removed {} hook {}",
        host.id,
        host.event,
        path.display()
    ))
}

/// One host's state from its rotter entries.
fn describe(host: &Host, ours: &[(Ours, &Value)], expected: u64) -> String {
    let install = format!("run `rotter integration install {}`", host.id);
    match ours {
        [] => "not installed".to_owned(),
        [(Ours::Current, only)] => {
            let timeout = &only[host.timeout_key];
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
        [(Ours::Legacy, _)] => {
            format!("installed (older command without `|| true`); {install}")
        }
        _ => format!(
            "installed for another binary: {}",
            ours.iter()
                .filter_map(|(_, entry)| entry[host.command_key].as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn merged_status(
    host: &Host,
    sources: &Sources,
    exe: &str,
    expected: u64,
) -> Result<String, String> {
    let path = hook_file(host, &host_dir(host, sources)?);
    let (settings, _) = read_settings(&path)?;
    let state = describe(host, &our_entries(host, &settings, exe), expected);
    Ok(format!("{}: {state} ({})", host.id, path.display()))
}

fn owned_status(
    host: &Host,
    sources: &Sources,
    exe: &str,
    expected: u64,
) -> Result<String, String> {
    let configured = host_dir(host, sources)?;
    let path = match trusted_home(host, &configured, user(), &lstat)? {
        Some(home) => owned_file(host, &home, false)?,
        None => None,
    };
    let Some(path) = path else {
        return Ok(format!(
            "not installed ({})",
            hook_file(host, &configured).display()
        ));
    };
    let handler = owned_installed(host, &path)?;
    let ours: Vec<(Ours, &Value)> = handler
        .iter()
        .map(|handler| {
            let how = if handler[host.command_key] == command(host, exe) {
                Ours::Current
            } else {
                Ours::Other
            };
            (how, handler)
        })
        .collect();
    Ok(format!(
        "{} ({})",
        describe(host, &ours, expected),
        path.display()
    ))
}

/// Whether Grok's Claude compatibility, which reads the literal `$HOME/.claude/settings.json`,
/// can pick up a rotter entry. Read-only and error-tolerant: anything unusual is "unknown".
/// Whether compat is enabled (`[compat.claude]`, `GROK_CLAUDE_HOOKS_ENABLED`) is not evaluated.
fn compat_status(sources: &Sources, exe: &str) -> Option<String> {
    let Some(home) = &sources.home else {
        return Some("grok: Claude compatibility entry unknown: HOME is not set".to_owned());
    };
    let path = home.join(".claude/settings.json");
    match read_settings(&path) {
        Ok((settings, _)) => (!our_entries(&CLAUDE, &settings, exe).is_empty()).then(|| {
            format!(
                "grok: found a Claude entry that Grok's Claude compatibility can pick up (whether \
                 compat is enabled was not checked) ({})",
                path.display()
            )
        }),
        Err(why) => Some(format!("grok: Claude compatibility entry unknown: {why}")),
    }
}

/// What Codex needs besides the entry: its `hooks` feature (on unless `[features]` in
/// `config.toml` turns off `hooks` or its older name `codex_hooks`) and a `/hooks` review, which
/// Codex records by the hook's hash and rotter does not check. Read-only; any problem reading
/// the file is "unknown".
fn codex_status(dir: &Path) -> String {
    let path = dir.join("config.toml");
    let feature = match open_regular(&path) {
        Ok(None) => "on (default)".to_owned(),
        Ok(Some(mut file)) => {
            let mut text = String::new();
            match file
                .read_to_string(&mut text)
                .map_err(|error| error.to_string())
                .and_then(|_| {
                    toml::from_str::<toml::Table>(&text).map_err(|error| error.to_string())
                }) {
                Ok(config) => {
                    let features = config.get("features").and_then(toml::Value::as_table);
                    let off = ["hooks", "codex_hooks"].iter().any(|key| {
                        features.and_then(|table| table.get(*key)) == Some(&false.into())
                    });
                    if off {
                        format!("off in {}; set [features] hooks = true", path.display())
                    } else {
                        "on".to_owned()
                    }
                }
                Err(why) => format!("unknown: {}: {why}", path.display()),
            }
        }
        Err(why) => format!("unknown: {why}"),
    };
    format!(
        "codex: hooks feature {feature}; Codex runs a new or changed hook only after you trust \
         it in /hooks (not checked)"
    )
}

/// Reports each host's hook state; `parse_timeout_seconds` gives the expected timeout.
pub fn status(parse_timeout_seconds: u64) -> Result<String, String> {
    let sources = Sources::from_env();
    let exe = exe();
    let expected = hook_timeout(parse_timeout_seconds);
    let mut lines = Vec::new();
    for host in hosts::HOSTS {
        let line = match host.install {
            // Claude's shared file's errors (e.g. a FIFO) fail status, as before; the other
            // hosts' are reported on their line.
            Install::MergeJson { .. } => match merged_status(host, &sources, &exe, expected) {
                Err(why) if host.id != CLAUDE.id => format!("{}: {why}", host.id),
                line => line?,
            },
            Install::OwnedJson { .. } => format!(
                "{}: {}",
                host.id,
                owned_status(host, &sources, &exe, expected).unwrap_or_else(|why| why)
            ),
        };
        lines.push(if host.experimental {
            format!("{line} [experimental]")
        } else {
            line
        });
        if host.id == hosts::CODEX.id
            && let Ok(dir) = host_dir(host, &sources)
        {
            lines.push(codex_status(&dir));
        }
    }
    lines.extend(compat_status(&sources, &exe));
    for (id, why) in hosts::UNSUPPORTED {
        lines.push(format!("{id}: unsupported: {why}"));
    }
    // The git a run in this directory would use, or why none is (hooks then stay silent).
    lines.push(format!(
        "git: {}",
        std::env::current_dir()
            .map_err(|error| error.to_string())
            .and_then(|dir| Git::cli(&dir))
            .map_or_else(|why| why, |git| git.summary())
    ));
    lines.push(match trusted_exe() {
        Ok(path) => format!("executable: {path} (safe to register)"),
        Err(why) => format!("executable: {why}"),
    });
    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::{
        Ours, Verdict, allow_request, entry_timeout, evaluate, faithful, hook_timeout, ours,
        owned_document, owned_handler, record, remove_ours, replace_file, reset_requests,
        rewritable, run_budget, session, trusted_home, verdict,
    };
    use crate::config::{Meta, Sources, lstat, user};
    use crate::hosts::{CLAUDE, COPILOT, DROID, GROK, Host};
    use serde_json::{Value, json};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};
    use std::time::Duration;

    const COMMAND: &str = "'/bin/rotter' hook claude-stop";
    const GROK_COMMAND: &str = "'/bin/rotter' hook grok-stop || true";

    fn settings(timeouts: &[Value]) -> Value {
        let hooks: Vec<Value> = timeouts
            .iter()
            .map(|timeout| json!({ "type": "command", "command": COMMAND, "timeout": timeout }))
            .collect();
        json!({ "hooks": { "Stop": [{ "hooks": hooks }] } })
    }

    fn installed(host: &Host, settings: &Value, command: &str) -> u64 {
        entry_timeout(host, settings, command).unwrap_or(host.default_timeout)
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
            run_budget(default, installed(&CLAUDE, &settings(timeouts), COMMAND))
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
            installed(&CLAUDE, &other, COMMAND),
            60,
            "another binary's entry"
        );
        assert_eq!(
            run_budget(
                hook_timeout(300),
                installed(&CLAUDE, &settings(&[json!(90)]), COMMAND)
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
                { "type": "command", "command": GROK_COMMAND, "timeout": timeout }
            ] }] } })
        };
        let grok = |document: &Value| {
            run_budget(hook_timeout(60), installed(&GROK, document, GROK_COMMAND))
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
            run_budget(hook_timeout(3600), GROK.default_timeout),
            Duration::from_secs(585)
        );
    }

    #[test]
    fn ownership_is_the_exact_rendered_command() {
        let exe = "/opt/it's/rotter";
        let quoted = r"'/opt/it'\''s/rotter'";
        let claude = |command: &str| ours(&CLAUDE, command, exe);
        assert_eq!(
            claude(&format!("{quoted} hook claude-stop || true")),
            Some(Ours::Current)
        );
        assert_eq!(
            claude(&format!("{quoted} hook claude-stop")),
            Some(Ours::Legacy)
        );
        for other in [
            "'/else/rotter' hook claude-stop || true",
            "'/else/rotter' hook claude-stop",
        ] {
            assert_eq!(claude(other), Some(Ours::Other), "{other}");
        }
        let foreign = [
            "'/x/rotter-proxy' hook codex",
            "'/opt/rotter-dev' hook claude-stop || true",
            "'/else/rotter' hook claude || true",
            "'/else/rotter' hook grok-stop || true",
            "'rotter' hook claude-stop || true",
            "/else/rotter hook claude-stop || true",
            "'/a' ; '/b/rotter' hook claude-stop || true",
            "'/else/rotter' hook claude-stop || true ",
            "'/else/rotter' hook claude-stop|| true",
            "'/opt/it's/rotter' hook claude-stop || true",
            "other",
        ];
        for command in foreign {
            assert_eq!(claude(command), None, "{command}");
        }
        // Grok never wrote the form without `|| true`.
        assert_eq!(ours(&GROK, "'/else/rotter' hook grok-stop", exe), None);
        assert_eq!(
            ours(&GROK, &format!("{quoted} hook grok-stop || true"), exe),
            Some(Ours::Current)
        );
    }

    #[test]
    fn uninstall_removes_only_groups_it_emptied() {
        let ours = json!({ "type": "command", "command": "'/b/rotter' hook claude-stop || true" });
        let mut settings = json!({ "hooks": { "Stop": [
            { "hooks": [] },
            { "matcher": "x", "hooks": [ours] },
            { "hooks": [ours, { "type": "command", "command": "other" }] },
            { "hooks": [{ "type": "command", "command": "'/x/rotter-proxy' hook codex" }] },
        ] } });
        assert_eq!(remove_ours(&CLAUDE, &mut settings, "/b/rotter"), 2);
        assert_eq!(
            settings,
            json!({ "hooks": { "Stop": [
                { "hooks": [] },
                { "hooks": [{ "type": "command", "command": "other" }] },
                { "hooks": [{ "type": "command", "command": "'/x/rotter-proxy' hook codex" }] },
            ] } })
        );
    }

    #[test]
    fn merged_files_must_survive_a_rewrite_unchanged() {
        let path = Path::new("/x/settings.json");
        let check = |text: &str| faithful(path, &serde_json::from_str(text).unwrap(), text);
        assert!(check(r#"{"a": [1, 2.5, 1e2, "x", null, true], "b": {"c": {}}}"#).is_ok());
        let error = check(r#"{"model": "a", "model": "b"}"#).unwrap_err();
        assert!(error.contains("duplicate key"), "{error}");
        assert!(check(r#"{"hooks": {"Stop": [{"x": 1, "x": 1}]}}"#).is_err());
    }

    #[test]
    fn merged_files_must_be_yours_and_not_writable_by_others() {
        let path = Path::new("/x/settings.json");
        assert!(rewritable(path, user(), 0o100644, user()).is_ok());
        assert!(rewritable(path, user(), 0o100600, user()).is_ok());
        let foreign = rewritable(path, user() + 1, 0o100644, user()).unwrap_err();
        assert!(foreign.contains("not owned by you"), "{foreign}");
        for mode in [0o100664, 0o100646, 0o100666] {
            let error = rewritable(path, user(), mode, user()).unwrap_err();
            assert!(
                error.contains("chmod go-w '/x/settings.json'"),
                "{mode:o}: {error}"
            );
        }
    }

    #[test]
    fn a_file_changed_after_reading_is_not_replaced() {
        let root = temp();
        let path = root.join("settings.json");
        fs::write(&path, "{\"b\": 1}\n").unwrap();
        let error = replace_file(&path, "{}\n", None, Some(Some(b"{\"a\": 1}\n"))).unwrap_err();
        assert!(error.contains("changed while rotter"), "{error}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "{\"b\": 1}\n");
        assert!(!root.join("settings.json.rotter-tmp").exists());
        // Created meanwhile, though missing when read.
        assert!(replace_file(&path, "{}\n", None, Some(None)).is_err());
        replace_file(&path, "{}\n", None, Some(Some(b"{\"b\": 1}\n"))).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "{}\n");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn only_the_exact_rotter_document_is_managed() {
        let handler = json!({ "type": "command", "command": GROK_COMMAND, "timeout": 90 });
        let document = |handlers: Value| json!({ "hooks": { "Stop": [{ "hooks": handlers }] } });
        let good = document(json!([handler]));
        assert_eq!(owned_handler(&GROK, &good), Some(&handler));
        let quoted = json!({ "type": "command", "timeout": 1,
            "command": r"'/opt/it'\''s/rotter' hook grok-stop || true" });
        assert!(owned_handler(&GROK, &document(json!([quoted]))).is_some());
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
            with("command", json!("'/bin/rotter' hook grok || true")),
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
            assert_eq!(owned_handler(&GROK, &document), None, "{document}");
        }
    }

    #[test]
    fn copilot_documents_are_flat_and_versioned() {
        let command = "'/bin/rotter' hook copilot || true";
        let good = owned_document(&COPILOT, command, 90);
        assert_eq!(
            good,
            json!({ "version": 1, "hooks": { "agentStop": [
                { "type": "command", "bash": command, "timeoutSec": 90 }
            ] } })
        );
        assert_eq!(
            owned_handler(&COPILOT, &good),
            Some(&good["hooks"]["agentStop"][0])
        );
        // The run budget reads the installed timeout under Copilot's own key.
        assert_eq!(installed(&COPILOT, &good, command), 90);
        assert_eq!(installed(&COPILOT, &json!({}), command), 30);
        assert_eq!(
            owned_document(&GROK, GROK_COMMAND, 90),
            json!({ "hooks": { "Stop": [{ "hooks": [
                { "type": "command", "command": GROK_COMMAND, "timeout": 90 }
            ] }] } })
        );
        let handler = good["hooks"]["agentStop"][0].clone();
        let with = |key: &str, value: Value| {
            let mut handler = handler.clone();
            handler[key] = value;
            json!({ "version": 1, "hooks": { "agentStop": [handler] } })
        };
        let rejected = [
            json!({ "hooks": { "agentStop": [handler] } }),
            json!({ "version": 2, "hooks": { "agentStop": [handler] } }),
            json!({ "version": "1", "hooks": { "agentStop": [handler] } }),
            json!({ "version": 1, "hooks": { "agentStop": [{ "hooks": [handler] }] } }),
            json!({ "version": 1, "hooks": { "agentStop": [handler, handler] } }),
            json!({ "version": 1, "hooks": { "Stop": [handler] } }),
            json!({ "version": 1, "hooks": { "agentStop": [handler] }, "x": 1 }),
            with("cwd", json!("/")),
            with("env", json!({})),
            with("timeoutSec", json!(0)),
            with("bash", json!("'/bin/rotter' hook copilot")),
            with("bash", json!("'/bin/rotter' hook grok-stop || true")),
            json!({ "version": 1, "hooks": { "agentStop": [
                { "type": "command", "command": command, "timeoutSec": 90 }
            ] } }),
            json!({ "version": 1, "hooks": { "agentStop": [
                { "type": "command", "bash": command, "timeout": 90 }
            ] } }),
        ];
        for document in rejected {
            assert_eq!(owned_handler(&COPILOT, &document), None, "{document}");
        }
        // A Grok document is not a Copilot one and back.
        assert_eq!(owned_handler(&GROK, &good), None);
    }

    #[test]
    fn droid_entries_sit_in_top_level_events() {
        let ours = json!({ "type": "command", "command": "'/b/rotter' hook droid || true" });
        let mut settings = json!({ "Stop": [
            { "hooks": [ours] },
            { "hooks": [{ "type": "command", "command": "/x.sh" }] },
        ], "hooks": { "Stop": [{ "hooks": [ours] }] } });
        assert_eq!(remove_ours(&DROID, &mut settings, "/b/rotter"), 1);
        // A nested "hooks" object is not Droid's hooks.json shape and stays.
        assert_eq!(
            settings,
            json!({ "Stop": [{ "hooks": [{ "type": "command", "command": "/x.sh" }] }],
                "hooks": { "Stop": [{ "hooks": [ours] }] } })
        );
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
    fn host_dirs_must_be_the_users_private_directory() {
        let root = temp();
        let home = root.join("grok");
        assert_eq!(
            trusted_home(&GROK, &home, user(), &lstat),
            Ok(None),
            "missing"
        );
        fs::create_dir(&home).unwrap();
        assert_eq!(
            trusted_home(&GROK, &home, user(), &lstat),
            Ok(Some(home.clone()))
        );
        // resolve_trusted accepts a root-owned sticky directory; a host dir does not.
        let sticky = with(home.clone(), 0, 0o41777);
        assert!(crate::config::resolve_trusted(&home, user(), &sticky).is_ok());
        for host in [&GROK, &CLAUDE] {
            // Root-owned: passes resolve_trusted, refused as not the user's.
            for mode in [0o41777, 0o40755] {
                let error =
                    trusted_home(host, &home, user(), &with(home.clone(), 0, mode)).unwrap_err();
                assert!(error.contains("owned by you"), "{mode:o}: {error}");
            }
            for mode in [0o40770, 0o40702] {
                assert!(
                    trusted_home(host, &home, user(), &with(home.clone(), user(), mode)).is_err()
                );
            }
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn session_ids_are_mapped_or_missing() {
        let id = |value: Value| session(&CLAUDE, &json!({ "session_id": value }));
        assert_eq!(id(json!("s/1 x")), Some("s_1_x".to_owned()));
        assert_eq!(id(json!("a".repeat(255))), Some("a".repeat(255)));
        for missing in [
            json!(""),
            json!("."),
            json!(".."),
            json!("a".repeat(256)),
            json!(7),
        ] {
            assert_eq!(id(missing.clone()), None, "{missing}");
        }
        assert_eq!(
            session(&GROK, &json!({ "sessionId": "g" })),
            Some("g".to_owned())
        );
        assert_eq!(session(&GROK, &json!({})), None);
    }

    #[test]
    fn the_loop_cap_allows_two_requests_then_one_silent_stop() {
        let state = temp().join("state/rotter");
        let asks: Vec<bool> = (0..7)
            .map(|_| allow_request(&state, &CLAUDE, "s"))
            .collect();
        assert_eq!(asks, [true, true, false, true, true, false, true]);
        // A Stop without a request resets; the counter is per host and session.
        reset_requests(&state, &CLAUDE, "s");
        assert!(allow_request(&state, &CLAUDE, "s") && allow_request(&state, &CLAUDE, "s"));
        assert!(allow_request(&state, &GROK, "s") && allow_request(&state, &CLAUDE, "t"));
        let count = state.join("claude-stop-count/s");
        assert_eq!(fs::read_to_string(&count).unwrap(), "2\n");
        assert_eq!(
            fs::metadata(&count).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // Fail closed: corrupt content, a directory in the way, an unwritable state dir.
        fs::write(&count, "x\n").unwrap();
        assert!(!allow_request(&state, &CLAUDE, "s"));
        reset_requests(&state, &CLAUDE, "s");
        assert!(allow_request(&state, &CLAUDE, "s"), "a reset repairs it");
        fs::create_dir(state.join("claude-stop-count/d")).unwrap();
        assert!(!allow_request(&state, &CLAUDE, "d"));
        let locked = temp();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).unwrap();
        assert!(!allow_request(&locked.join("state"), &CLAUDE, "s"));
        assert!(!allow_request(&locked, &CLAUDE, "s"));
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
        // A reset never creates anything.
        let fresh = temp().join("state");
        reset_requests(&fresh, &CLAUDE, "s");
        assert!(!fresh.exists());
    }

    #[test]
    fn concurrent_stops_never_lose_a_count() {
        // (preset count, concurrent stops) → requests that may pass.
        for (preset, stops, passes) in [(Some("1\n"), 2, 1), (None, 2, 2), (Some("2\n"), 3, 2)] {
            let state = temp();
            if let Some(preset) = preset {
                fs::create_dir(state.join("claude-stop-count")).unwrap();
                fs::write(state.join("claude-stop-count/s"), preset).unwrap();
            }
            let barrier = Arc::new(Barrier::new(stops));
            let threads: Vec<_> = (0..stops)
                .map(|_| {
                    let (state, barrier) = (state.clone(), Arc::clone(&barrier));
                    std::thread::spawn(move || {
                        barrier.wait();
                        allow_request(&state, &CLAUDE, "s")
                    })
                })
                .collect();
            let passed = threads
                .into_iter()
                .map(|thread| usize::from(thread.join().unwrap()))
                .sum::<usize>();
            assert_eq!(passed, passes, "{preset:?} x{stops}");
            fs::remove_dir_all(state).unwrap();
        }
    }

    /// Replays Stops of (host, report, units) in one session; returns how many blocked.
    fn replay(turns: &[(&Host, &str, usize)]) -> usize {
        let state = temp();
        let mut blocks = 0;
        for (host, report, found) in turns {
            match verdict(Some(&state), host, "s", report, *found) {
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
        let (claude, grok) = (&CLAUDE, &GROK);
        // Native Grok sees units; its Claude-compat twin times out with none. Three user turns
        // (continuations are skipped before any of this).
        let turn = [(grok, "R", 3), (claude, "Z", 0)];
        assert_eq!(replay(&turn.repeat(3)), 1);
        // Different budgets, different reports with units: each once.
        let turn = [(grok, "R1", 3), (claude, "R2", 2)];
        assert_eq!(replay(&turn.repeat(3)), 2);
        // Either order.
        assert_eq!(
            replay(&[(claude, "R", 1), (grok, "R", 1), (claude, "R", 1)]),
            1
        );
        // One host alone, A→B→A: the other slot is empty, so A is requested again.
        assert_eq!(replay(&[(grok, "A", 1), (grok, "B", 1), (grok, "A", 1)]), 3);
        // A→B→A is quiet when the other host's last block was A.
        assert_eq!(
            replay(&[
                (claude, "A", 1),
                (grok, "A", 1),
                (grok, "B", 1),
                (grok, "A", 1)
            ]),
            2
        );
        // No state directory: no dedupe (and the loop cap keeps the request from going out).
        assert_eq!(verdict(None, grok, "s", "R", 1), Verdict::Block(None));
    }

    /// The native git helper (tests/common/git_helper.rs), compiled once into the temp dir.
    fn helper() -> PathBuf {
        static HELPER: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        HELPER
            .get_or_init(|| {
                let dir = temp();
                let source = dir.join("git_helper.rs");
                fs::write(&source, include_str!("../tests/common/git_helper.rs")).unwrap();
                let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
                let output = Command::new(rustc)
                    .args(["--edition", "2024", "-o"])
                    .arg(dir.join("helper"))
                    .arg(&source)
                    .output()
                    .unwrap();
                assert!(output.status.success(), "{output:?}");
                dir.join("helper")
            })
            .clone()
    }

    const CHILD: &str = "ROTTER_TEST_INJECTED_CHILD";

    /// The hook core with injected sources, run in a child test process whose environment names
    /// other directories for every variable: config, state, temp, HOME and PATH all come from
    /// the injected sources. Nothing is read from or written to the real home.
    #[test]
    fn injected_sources_are_the_only_environment_on_the_hook_path() {
        let Some(root) = std::env::var_os(CHILD).map(PathBuf::from) else {
            let root = temp();
            let evil = root.join("evil");
            fs::create_dir_all(evil.join(".config/rotter")).unwrap();
            // A grammar-enabling config and a permissive git config under the env-named HOME.
            fs::write(
                evil.join(".config/rotter/config.toml"),
                "languages = [\"python\"]\n",
            )
            .unwrap();
            fs::write(evil.join(".gitconfig"), "[safe]\n\tdirectory = *\n").unwrap();
            // The recording helper, the only git on the injected PATH, is prepared here: the
            // child's own temp dir is one of the env-named directories.
            let bin = root.join("bin");
            fs::create_dir(&bin).unwrap();
            fs::copy(helper(), bin.join("git")).unwrap();
            let real = std::env::split_paths(&std::env::var_os("PATH").unwrap())
                .map(|dir| dir.join("git"))
                .find(|path| path.is_absolute() && path.is_file())
                .unwrap();
            fs::write(
                bin.join("git.conf"),
                format!(
                    "env {}\nreal {}\n",
                    root.join("git.env").display(),
                    real.display()
                ),
            )
            .unwrap();
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "integration::tests::injected_sources_are_the_only_environment_on_the_hook_path",
                    "--test-threads=1",
                ])
                .env(CHILD, &root)
                .env("HOME", &evil)
                .env("XDG_CONFIG_HOME", evil.join(".config"))
                .env("XDG_CACHE_HOME", evil.join(".cache"))
                .env("XDG_STATE_HOME", evil.join(".state"))
                .env("ROTTER_STATE_DIR", evil.join("state"))
                .env("TMPDIR", evil.join("tmp"))
                .env("CLAUDE_CONFIG_DIR", evil.join("claude"))
                .env("CODEX_HOME", evil.join("codex"))
                .env("COPILOT_HOME", evil.join("copilot"))
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            assert!(
                String::from_utf8_lossy(&output.stdout).contains("1 passed"),
                "{output:?}"
            );
            let entries: Vec<_> = fs::read_dir(&evil)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            assert_eq!(
                entries.len(),
                2,
                "nothing written under the env-named dirs: {entries:?}"
            );
            fs::remove_dir_all(root).unwrap();
            return;
        };
        let git = |dir: &Path, args: &[&str]| {
            let status = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
                .status()
                .unwrap();
            assert!(status.success(), "{args:?}");
        };
        let repo = root.join("repo");
        fs::create_dir(&repo).unwrap();
        fs::write(
            repo.join("a.go"),
            "package p\n\n// F.\nfunc F() int { return 1 }\n",
        )
        .unwrap();
        fs::write(repo.join("b.py"), "# G.\ndef g():\n    return 1\n").unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "c"]);
        fs::write(
            repo.join("a.go"),
            "package p\n\n// F.\nfunc F() int { return 2 }\n",
        )
        .unwrap();
        fs::write(repo.join("b.py"), "# G.\ndef g():\n    return 2\n").unwrap();
        let bin = root.join("bin");
        let log = root.join("git.env");
        let passwd = root.join("passwd-home");
        let temp_root = root.join("tmp");
        fs::create_dir_all(&passwd).unwrap();
        fs::create_dir(&temp_root).unwrap();
        fs::set_permissions(&temp_root, fs::Permissions::from_mode(0o700)).unwrap();
        let sources = Sources {
            home: Some(passwd.clone()),
            config: Some(passwd.join(".config")),
            cache: Some(passwd.join(".cache")),
            state: Some(passwd.join(".local/state/rotter")),
            temp: temp_root.clone(),
            path: Some(bin.clone().into_os_string()),
            vars: Vec::new(),
        };
        let input = json!({ "session_id": "i", "cwd": repo });
        let outcome = evaluate(&CLAUDE, &input, "i", &sources).unwrap();
        let reason = outcome.reason.expect("a review request");
        // The env-named config would enable python and leave b.py unanalysed.
        assert!(reason.contains("report complete: true"), "{reason}");
        assert!(outcome.messages.is_empty(), "{:?}", outcome.messages);
        let calls = std::fs::read_to_string(&log).unwrap();
        let calls: Vec<&str> = calls
            .split("--\n")
            .filter(|call| !call.is_empty())
            .collect();
        assert!(calls.len() > 3, "{calls:?}");
        for call in calls {
            let home = format!("\nHOME={}\n", passwd.display());
            assert!(call.contains(&home), "{call}");
            assert!(
                call.contains(&format!("\nTMPDIR={}/rotter-", temp_root.display())),
                "{call}"
            );
            assert!(!call.contains("evil"), "{call}");
        }
    }
}
