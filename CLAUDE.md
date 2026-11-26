# rotter

## Verify every code change

The tools and tasks are pinned in `mise.toml`; `mise install` installs them.

- After every code change, run `mise run verify`: format (`fmt`), compile (`build`), lint (`lint`:
  clippy and actionlint) and fast static analysis (`sast`: opengrep with `.opengrep/` and pinned
  community rules, cargo-deny with `deny.toml`, zizmor). Fix everything it reports before going on.
- Before every commit, run `mise run verify-full`: `verify`, then `codeql` (CodeQL security queries
  on the Rust code, the shims' JavaScript and the workflows), `crap` (coverage, then no function
  may score above CRAP 30) and `mutants` (every mutant of the change since `main` must be caught; a mutant that makes the tests
  time out counts as caught).
- A failing task is a finding: fix the code or add the missing test. Change a threshold or an
  exclusion (`.cargo/mutants.toml`, `.opengrep/`, `deny.toml`, `.codeql/suppressions.json`, the
  alert filter in the `codeql` task) only after the user agrees, and write the reason next to it.
- Report which tasks ran and how they ended. Never say a task passed without running it.

CI runs three of these gates on pull requests, each only when its files change: `mutants` and
`crap` for the Rust code and tests, `workflows` (actionlint, zizmor) for `.github/` and the tool
versions in `mise.toml`. Formatting, Clippy, opengrep, cargo-deny and CodeQL run only locally.
