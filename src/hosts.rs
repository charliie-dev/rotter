//! The agents rotter integrates with as a turn-end hook: where each keeps its hook, what its hook
//! input looks like and how it takes a reply. Every host-specific fact lives in this table; the
//! runtime and the install code only read it. `docs/hosts.md` records where each fact comes from.

/// How rotter's hook is registered with a host.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Install {
    /// One handler in a `{"hooks":[handler]}` group of the event's array in `<host dir>/<file>`,
    /// a JSON file the host and other tools share. With `nested`, the event map is the file's
    /// `"hooks"` object (`{"hooks":{"Stop":[…]}}`), else the file itself (`{"Stop":[…]}`).
    MergeJson { file: &'static str, nested: bool },
    /// `<host dir>/<dir>/<file>`, a JSON document rotter owns entirely; `<dir>` is created (0700)
    /// inside an existing host dir when missing. The document is
    /// `{["version":V,]"hooks":{"<event>":[H]}}`, with H the handler itself or, when `grouped`,
    /// `{"hooks":[handler]}`.
    OwnedJson {
        dir: &'static str,
        file: &'static str,
        version: Option<u64>,
        grouped: bool,
    },
    /// `<host dir>/<dir>/<file>`, a code file the host loads into its own process at start,
    /// rendered from an embedded template with exactly the exe path and the timeout. `<dir>` is
    /// created (0700) inside an existing host dir when missing. `templates` holds every version
    /// rotter has shipped, the current one first: a file equal to the render of any of them
    /// (for some accepted exe and timeout) is rotter's, anything else is foreign. With
    /// `max_timeout`, the host awaits the handler, so the timeout is capped.
    OwnedShim {
        dir: &'static str,
        file: &'static str,
        templates: &'static [&'static str],
        max_timeout: Option<u64>,
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
    /// Not run against the real host by rotter's authors; `status` says so.
    pub(crate) experimental: bool,
    /// The environment variable naming the host's directory (used only when absolute), else
    /// `<home>/<fallback>` (or under the XDG config base); None when the host documents none.
    pub(crate) dir_var: Option<&'static str>,
    pub(crate) fallback: &'static str,
    /// Whether `fallback` is under the XDG config base (`$XDG_CONFIG_HOME` when absolute, else
    /// `~/.config`) rather than the home.
    pub(crate) fallback_in_config: bool,
    pub(crate) install: Install,
    /// The hook event the entry is registered under.
    pub(crate) event: &'static str,
    /// The handler keys holding the shell command and the timeout in seconds.
    pub(crate) command_key: &'static str,
    pub(crate) timeout_key: &'static str,
    /// Other files in the host dir whose `"hooks"` the host reads only while `<file>` is missing,
    /// so creating it would silently disable them: install then refuses.
    pub(crate) shadows: &'static [&'static str],
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
    experimental: false,
    dir_var: Some("CLAUDE_CONFIG_DIR"),
    fallback: ".claude",
    fallback_in_config: false,
    install: Install::MergeJson {
        file: "settings.json",
        nested: true,
    },
    event: "Stop",
    command_key: "command",
    timeout_key: "timeout",
    shadows: &[],
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
    experimental: false,
    dir_var: Some("GROK_HOME"),
    fallback: ".grok",
    fallback_in_config: false,
    install: Install::OwnedJson {
        dir: "hooks",
        file: "rotter.json",
        version: None,
        grouped: true,
    },
    event: "Stop",
    command_key: "command",
    timeout_key: "timeout",
    shadows: &[],
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

/// OpenAI Codex CLI: `$CODEX_HOME/hooks.json` Stop hooks, run once trusted in `/hooks`.
pub(crate) const CODEX: Host = Host {
    id: "codex",
    hook: "codex",
    label: "Codex",
    experimental: true,
    dir_var: Some("CODEX_HOME"),
    fallback: ".codex",
    fallback_in_config: false,
    install: Install::MergeJson {
        file: "hooks.json",
        nested: true,
    },
    event: "Stop",
    command_key: "command",
    timeout_key: "timeout",
    shadows: &[],
    legacy: false,
    session_keys: &["session_id"],
    cwd_keys: &["cwd"],
    cwd_fallback: false,
    continuation_keys: &["stop_hook_active"],
    end_reason: None,
    notes: Notes::SystemMessage,
    default_timeout: 600,
    injectable: false,
};

/// GitHub Copilot CLI: `$COPILOT_HOME/hooks/*.json` agentStop hooks.
pub(crate) const COPILOT: Host = Host {
    id: "copilot",
    hook: "copilot",
    label: "GitHub Copilot CLI",
    experimental: false,
    dir_var: Some("COPILOT_HOME"),
    fallback: ".copilot",
    fallback_in_config: false,
    install: Install::OwnedJson {
        dir: "hooks",
        file: "rotter.json",
        version: Some(1),
        grouped: false,
    },
    event: "agentStop",
    command_key: "bash",
    timeout_key: "timeoutSec",
    shadows: &[],
    legacy: false,
    session_keys: &["sessionId", "session_id"],
    cwd_keys: &["cwd"],
    cwd_fallback: false,
    continuation_keys: &["stop_hook_active"],
    end_reason: Some(("stopReason", "end_turn")),
    notes: Notes::Stderr,
    default_timeout: 30,
    injectable: false,
};

/// Factory Droid: `~/.factory/hooks.json` Stop hooks (no directory variable).
pub(crate) const DROID: Host = Host {
    id: "droid",
    hook: "droid",
    label: "Factory Droid",
    experimental: true,
    dir_var: None,
    fallback: ".factory",
    fallback_in_config: false,
    install: Install::MergeJson {
        file: "hooks.json",
        nested: false,
    },
    event: "Stop",
    command_key: "command",
    timeout_key: "timeout",
    // Droid reads `hooks` from these only while hooks.json is absent.
    shadows: &["settings.json", "settings.local.json"],
    legacy: false,
    session_keys: &["session_id"],
    cwd_keys: &["cwd"],
    cwd_fallback: false,
    continuation_keys: &["stop_hook_active"],
    end_reason: None,
    notes: Notes::Stderr,
    default_timeout: 60,
    injectable: false,
};

/// Pi: `$PI_CODING_AGENT_DIR/extensions/*.ts`, an `agent_before_settle` extension.
pub(crate) const PI: Host = Host {
    id: "pi",
    hook: "pi",
    label: "Pi",
    experimental: true,
    dir_var: Some("PI_CODING_AGENT_DIR"),
    fallback: ".pi/agent",
    fallback_in_config: false,
    install: Install::OwnedShim {
        dir: "extensions",
        file: "rotter-review.ts",
        templates: &[include_str!("shims/pi-v1.ts")],
        max_timeout: Some(120),
    },
    event: "agent_before_settle",
    command_key: "",
    timeout_key: "",
    shadows: &[],
    legacy: false,
    // The shim writes exactly these; it skips runs that did not complete.
    session_keys: &["session_id"],
    cwd_keys: &["cwd"],
    cwd_fallback: false,
    continuation_keys: &[],
    end_reason: None,
    notes: Notes::Stderr,
    default_timeout: 120,
    // The shim passes only PATH and LANG, and Pi's Bun build loads a project `.env`.
    injectable: true,
};

/// Letta Code: `~/.letta/mods/*.js`, a `turn_end` mod (no directory variable reaches its loader).
pub(crate) const LETTA: Host = Host {
    id: "letta",
    hook: "letta",
    label: "Letta Code",
    experimental: true,
    dir_var: None,
    fallback: ".letta",
    fallback_in_config: false,
    install: Install::OwnedShim {
        dir: "mods",
        file: "rotter-review.js",
        templates: &[include_str!("shims/letta-v1.js")],
        max_timeout: Some(120),
    },
    event: "turn_end",
    command_key: "",
    timeout_key: "",
    shadows: &[],
    legacy: false,
    session_keys: &["session_id"],
    cwd_keys: &["cwd"],
    cwd_fallback: false,
    continuation_keys: &[],
    end_reason: None,
    notes: Notes::Stderr,
    default_timeout: 120,
    injectable: true,
};

/// OpenCode: `$OPENCODE_CONFIG_DIR/plugins/*.js` (a directory OpenCode loads plugins from in
/// addition to `$XDG_CONFIG_HOME/opencode`), a plugin reacting to `session.idle`.
pub(crate) const OPENCODE: Host = Host {
    id: "opencode",
    hook: "opencode",
    label: "OpenCode",
    experimental: true,
    dir_var: Some("OPENCODE_CONFIG_DIR"),
    fallback: "opencode",
    fallback_in_config: true,
    install: Install::OwnedShim {
        dir: "plugins",
        file: "rotter-review.js",
        templates: &[include_str!("shims/opencode-v1.js")],
        // OpenCode does not await plugin event handlers.
        max_timeout: None,
    },
    event: "session.idle",
    command_key: "",
    timeout_key: "",
    shadows: &[],
    legacy: false,
    session_keys: &["session_id"],
    cwd_keys: &["cwd"],
    cwd_fallback: false,
    continuation_keys: &[],
    end_reason: None,
    notes: Notes::Stderr,
    default_timeout: 120,
    injectable: true,
};

pub(crate) const HOSTS: [&Host; 8] = [
    &CLAUDE, &GROK, &CODEX, &COPILOT, &DROID, &PI, &LETTA, &OPENCODE,
];

/// Hosts whose contract addendum (`docs/hosts.md`) could not be completed, with the reason.
/// omp, kilo and hermes are not researched further (user decision, S4): "not supported yet
/// (TODO)" stands in for a completed addendum. China-based agents (kimi, qwen, qodercli) are
/// deliberately out of scope and never appear here or in `HOSTS` (see `docs/hosts.md`).
pub(crate) const UNSUPPORTED: [(&str, &str); 7] = [
    ("omp", "not supported yet (TODO)"),
    ("kilo", "not supported yet (TODO)"),
    ("hermes", "not supported yet (TODO)"),
    (
        "mastracode",
        "Mastra Code continues a Stop only on exit code 2, which the `|| true` command form rules \
         out",
    ),
    (
        "devin",
        "Devin CLI does not document whether hook commands run through a shell, their default \
         timeout or the hook's working directory",
    ),
    (
        "cursor",
        "Cursor appends the hook input as a here-document to the command, so behind `|| true` \
         rotter would not receive it",
    ),
    (
        "antigravity-cli",
        "Antigravity CLI does not document how hook commands are run or what their exit codes \
         mean",
    ),
];

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

/// `claude (claude-stop), grok (grok-stop), codex, …`, for usage errors.
pub fn names() -> String {
    HOSTS
        .iter()
        .map(|host| {
            if host.hook == host.id {
                host.id.to_owned()
            } else {
                format!("{} ({})", host.id, host.hook)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::{
        CLAUDE, CODEX, COPILOT, DROID, GROK, HOSTS, Install, LETTA, OPENCODE, PI, UNSUPPORTED,
        by_hook, by_id,
    };

    #[test]
    fn hook_names_and_ids_map_to_one_host() {
        for (name, host) in [
            ("claude", &CLAUDE),
            ("claude-stop", &CLAUDE),
            ("grok", &GROK),
            ("grok-stop", &GROK),
            ("codex", &CODEX),
            ("copilot", &COPILOT),
            ("droid", &DROID),
            ("pi", &PI),
            ("letta", &LETTA),
            ("opencode", &OPENCODE),
        ] {
            assert_eq!(by_hook(name).map(|found| found.id), Some(host.id), "{name}");
        }
        for name in [
            "bogus",
            "",
            "Claude",
            "claude-stop ",
            "codex-stop",
            "cursor",
            "devin",
            "opencode-stop",
        ] {
            assert!(by_hook(name).is_none(), "{name:?}");
        }
        assert!(by_id("claude-stop").is_none(), "install targets are ids");
        assert_eq!(by_id("grok").map(|host| host.hook), Some("grok-stop"));
        for (name, _) in UNSUPPORTED {
            assert!(by_id(name).is_none() && by_hook(name).is_none(), "{name}");
        }
    }

    #[test]
    fn ids_and_hook_names_are_unique() {
        for (index, host) in HOSTS.iter().enumerate() {
            for other in &HOSTS[index + 1..] {
                assert_ne!(host.hook, other.hook);
                assert_ne!(host.id, other.id);
            }
            // Timeouts are written in whole seconds under the host's own key (a shim's is in
            // its code).
            let shim = matches!(host.install, Install::OwnedShim { .. });
            assert!(host.default_timeout > 0 && (shim || !host.timeout_key.is_empty()));
        }
    }
}
