#!/usr/bin/env bash
# Claude Code Stop hook: when the working tree has changes that rotter relates to comments,
# block once and ask the agent to review them with the rotter-comment-review skill.
# Never blocks a continuation it caused, and asks again only after the report changes.
set -euo pipefail

message() { jq -n --arg text "rotter: $1" '{systemMessage: $text}'; }

if ! command -v jq >/dev/null; then
  echo '{"systemMessage": "rotter: jq is required by the Stop hook"}'
  exit 0
fi
input=$(cat)
# Claude Code sends stop_hook_active; Grok Build sends stopHookActive.
if [[ $(jq -r '.stop_hook_active // .stopHookActive // false' <<<"$input") == true ]]; then
  exit 0
fi
cwd=$(jq -r '.cwd // empty' <<<"$input")
cwd=${cwd:-$PWD}
session=$(jq -r '.session_id // .sessionId // "unknown"' <<<"$input" | tr -c 'A-Za-z0-9._\n-' _)
git -C "$cwd" rev-parse --is-inside-work-tree >/dev/null 2>&1 || exit 0

rotter=${ROTTER_BIN:-rotter}
command=("$rotter" extract --worktree --include-untracked -C "$cwd")
errors=$(mktemp "${TMPDIR:-/tmp}/rotter-hook.XXXXXX")
trap 'rm -f "$errors"' EXIT
status=0
report=$("${command[@]}" 2>"$errors") || status=$?
if ((status > 1)) || ! jq -e .files >/dev/null 2>&1 <<<"$report"; then
  message "extract failed (exit $status): $(head -c 500 "$errors")"
  exit 0
fi

units=$(jq '[.files[] | .before, .after | .units? // [] | .[]] | length' <<<"$report")
complete=$(jq '.complete' <<<"$report")
if ((units == 0)); then
  [[ $complete == true ]] || message "some changed files could not be analysed; run ${command[*]}"
  exit 0
fi

state_dir="${ROTTER_STATE_DIR:-${XDG_STATE_HOME:-$HOME/.local/state}/rotter}/claude-stop"
mkdir -p "$state_dir"
fingerprint=$(printf '%s\n%s' "$cwd" "$report" | shasum -a 256 | cut -d' ' -f1)
if [[ -f "$state_dir/$session" && $(<"$state_dir/$session") == "$fingerprint" ]]; then
  exit 0
fi
printf '%s\n' "$fingerprint" >"$state_dir/$session"

jq -n --arg reason "rotter found $units changed code unit(s) with related comments in $cwd \
(report complete: $complete). Before finishing, review them with the rotter-comment-review skill \
in working tree mode including untracked files: run \`${command[*]}\`. If that skill is not \
loaded, run \`$rotter --skill\` and follow its output. Report only concrete contradictions between comments and code; \
do not edit files unless the user asked for it." '{decision: "block", reason: $reason}'
