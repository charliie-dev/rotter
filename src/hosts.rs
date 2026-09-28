//! The agents rotter integrates with as a turn-end hook: where each keeps its hook, what its hook
//! input looks like and how it takes a reply. Every host-specific fact lives in this table; the
//! runtime and the install code only read it. `docs/hosts.md` records where each fact comes from.

/// How rotter's hook is registered with a host.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Install {
    /// One entry in `<host dir>/<file>`, a JSON file the host and other tools share.
    MergeJson { file: &'static str },
    /// `<host dir>/<dir>/<file>`, a JSON document rotter owns entirely; `<dir>` is created (0700)
    /// inside an existing host dir when missing.
    OwnedJson {
        dir: &'static str,
        file: &'static str,
    },
}

/// Where messages that do not block go.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Notes {
    /// A `systemMessage` in the JSON reply, shown once per session.
    SystemMessage,
    /// stderr; the host does not show it, so it is never marked as shown.
    Stderr,
}

#[derive(Debug)]
pub struct Host {
    /// The host id: install targets, messages and loop-cap keys.
    pub(crate) id: &'static str,
    /// The name `rotter hook <name>` is rendered with, also the prefix of the host's state
    /// directories (`claude-stop`, `claude-stop-notes`, …), kept from the first release.
    pub(crate) hook: &'static str,
    pub(crate) label: &'static str,
    /// The environment variable naming the host's directory (used only when absolute), else
    /// `<home>/<fallback>`.
    pub(crate) dir_var: &'static str,
    pub(crate) fallback: &'static str,
    pub(crate) install: Install,
    /// The hook event the entry is registered under.
    pub(crate) event: &'static str,
    /// Whether install also recognises the command without ` || true` (the pre-S0 form).
    pub(crate) legacy: bool,
    pub(crate) session_keys: &'static [&'static str],
    pub(crate) cwd_keys: &'static [&'static str],
    /// Whether a Stop without a usable cwd key runs in the hook process's own directory.
    pub(crate) cwd_fallback: bool,
    /// Keys whose `true` marks a continuation the host started because of a block.
    pub(crate) continuation_keys: &'static [&'static str],
    /// `(key, value)`: a Stop whose `key` is present and not `value` ends no turn.
    pub(crate) end_reason: Option<(&'static str, &'static str)>,
    pub(crate) notes: Notes,
    /// The host's own Stop timeout in seconds, assumed when the installed one is unusable.
    pub(crate) default_timeout: u64,
    /// Whether project configuration can set the hook process's environment in ways the host's
    /// own children do not share; such hosts take HOME from the password database and ignore
    /// XDG_*, ROTTER_* and TMPDIR (see `Sources::injectable`).
    pub(crate) injectable: bool,
}

/// Claude Code: `settings.json` Stop hooks.
pub(crate) const CLAUDE: Host = Host {
    id: "claude",
    hook: "claude-stop",
    label: "Claude Code",
    dir_var: "CLAUDE_CONFIG_DIR",
    fallback: ".claude",
    install: Install::MergeJson {
        file: "settings.json",
    },
    event: "Stop",
    legacy: true,
    // Grok's Claude compatibility runs this hook with Grok's camelCase input.
    session_keys: &["session_id", "sessionId"],
    cwd_keys: &["cwd"],
    cwd_fallback: true,
    continuation_keys: &["stop_hook_active", "stopHookActive"],
    end_reason: Some(("reason", "end_turn")),
    notes: Notes::SystemMessage,
    default_timeout: 60,
    injectable: false,
};

/// Grok Build: native `hooks/*.json` Stop hooks.
pub(crate) const GROK: Host = Host {
    id: "grok",
    hook: "grok-stop",
    label: "Grok Build",
    dir_var: "GROK_HOME",
    fallback: ".grok",
    install: Install::OwnedJson {
        dir: "hooks",
        file: "rotter.json",
    },
    event: "Stop",
    legacy: false,
    session_keys: &["session_id", "sessionId"],
    cwd_keys: &["cwd", "workspaceRoot"],
    cwd_fallback: false,
    continuation_keys: &["stop_hook_active", "stopHookActive"],
    // The session-end Stop (`channel_closed`, `shutdown`) ends no turn; its decision is ignored.
    end_reason: Some(("reason", "end_turn")),
    notes: Notes::Stderr,
    default_timeout: 600,
    injectable: false,
};

pub(crate) const HOSTS: [&Host; 2] = [&CLAUDE, &GROK];

/// The host of `rotter hook <name>`: its id or its rendered hook name, so `claude` and
/// `claude-stop` are one host (one counter, one dedupe slot).
pub fn by_hook(name: &str) -> Option<&'static Host> {
    HOSTS
        .into_iter()
        .find(|host| host.id == name || host.hook == name)
}

/// The host an `integration` command names, by id.
pub(crate) fn by_id(name: &str) -> Option<&'static Host> {
    HOSTS.into_iter().find(|host| host.id == name)
}

/// `claude (claude-stop), grok (grok-stop)`, for usage errors.
pub fn names() -> String {
    HOSTS
        .iter()
        .map(|host| format!("{} ({})", host.id, host.hook))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::{CLAUDE, GROK, by_hook, by_id};

    #[test]
    fn hook_names_and_ids_map_to_one_host() {
        for (name, host) in [
            ("claude", &CLAUDE),
            ("claude-stop", &CLAUDE),
            ("grok", &GROK),
            ("grok-stop", &GROK),
        ] {
            assert_eq!(by_hook(name).map(|found| found.id), Some(host.id), "{name}");
        }
        for name in ["bogus", "", "Claude", "claude-stop ", "codex"] {
            assert!(by_hook(name).is_none(), "{name:?}");
        }
        assert!(by_id("claude-stop").is_none(), "install targets are ids");
        assert_eq!(by_id("grok").map(|host| host.hook), Some("grok-stop"));
    }
}
