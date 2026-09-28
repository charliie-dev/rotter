# Rotter

針對 Git diff 檢查程式註解的 Rust 專案。目前是 POC：`rotter extract` 擷取變更單元與相關註解，
輸出 JSON；語意判斷交給 coding agent 依 [共用 skill](skills/rotter-comment-review/SKILL.md) 執行。
Claude Code 可透過 [Stop hook](hooks/claude-stop.sh) 自動發起審查。

## 開發環境

Rust 工具鏈、rustfmt 與 Clippy 由 [mise.toml](mise.toml) 固定並管理。
Rust 套件依賴由 [Cargo.toml](Cargo.toml)／[Cargo.lock](Cargo.lock) 固定。
宿主需要已有 Git 與可用的 C 編譯器／平台 SDK，因為 Tree-sitter 會編譯原生 C 程式。

```sh
mise install rust
mise run check
```

`check` 會執行 Rust 格式檢查、Clippy 和全部測試。任務不會自動安裝從全域 mise 設定
繼承的工具，因此首次使用須先執行上述安裝命令。

也可分別執行：

```sh
mise run fmt
mise run lint
mise run test-parsers
```

## 使用方式（POC）

```sh
cargo run --locked -- extract --worktree            # HEAD → 工作目錄（已暫存＋未暫存）
cargo run --locked -- extract --staged              # HEAD → index
cargo run --locked -- extract --base <rev>          # 指定版本 → 工作目錄
cargo run --locked -- extract --worktree --include-untracked -C <repo>
cargo run --locked -- extract --full -- src/ config/  # 全部追蹤檔案（工作目錄），不看 diff
```

`--` 後的 pathspec 適用所有模式，相對於目前目錄；不給就是整個 repo。

必須明確選一種模式，不會自動猜基準。退出碼：`0` 完整、`1` 已輸出 JSON 但有檔案未能完整分析
（語法錯誤、非 UTF-8、含 NUL、zsh／ksh 腳本、衝突中的路徑等）、`2` 參數或 Git 錯誤。
有語法錯誤時狀態為 `partial`：仍輸出能解析的單元，與錯誤重疊者標 `overlaps_syntax_error`。

`--full` 以「每則註解」為起點，列出它所屬或說明的單元，每則註解只出現一次；沒有 before 側，
超過 80 行的單元原文會截斷（`text_truncated`）。輸出超過 5 MiB 時會在 stderr 提醒用 pathspec 縮小範圍。
`#!/bin/sh`、dash、ash 腳本以 Bash grammar 解析，dialect 標為 `*-parsed-as-bash`。
JSON schema 標為 `rotter.extract.poc/0`，仍可能變動。

每個變更檔案的 before／after 各自列出：

- `units`：變更所在的宣告、函式、binding 或設定鍵，含原文與範圍；
  另列出同檔案中使用變更名稱的單元（`selected_by: "reference"`，每個名稱最多 20 個）。
- `comments`：`leading`（緊接上方，Rust attribute 行可夾在中間）、`inside`、`trailing`
  （同一行尾）、`enclosing_leading`（外層單元上方）。相鄰的同類行註解合併成一個區塊；
  Go 指示、Lua 型別標註、ShellCheck 指示、shebang 等另標 `directive`，不與一般註解合併。
- `range.bytes` 是 UTF-8 byte offset（結尾不含），`range.lines` 是 1 起算的閉區間；
  `text` 必須等於快照在該範圍的原文。`blob` 是該快照的 Git blob id，可用來核對。

CLI 只讀：Git 以 `GIT_OPTIONAL_LOCKS=0`、`core.fsmonitor=false` 執行，並透過私有 index 副本
（`GIT_INDEX_FILE`）讀取，避免 `git diff` refresh 時重寫 `.git/index`；diff 輸入寫入
權限 0700 的暫存目錄並在結束時刪除。只讀取七種語言的檔案內容；其他檔案只列路徑。

## 目前可驗證的範圍

`parse(Language, &str)` 選擇 Go、Lua、Nix、Bash、YAML、TOML 或 Rust grammar，
回傳語法樹或明確的解析錯誤。語法樹包含錯誤時，回傳 `ParseError::Syntax`。
原文範圍沿用 Tree-sitter 的 byte offset 與零起算 row／column，不自行改寫換行。

[parser 測試](tests/parsers.rs) 涵蓋：

- 七語言的變更前後固定檔案：程式值改變、註解保持原樣。
- 字串、Bash heredoc、Lua 長字串及 YAML block scalar 中的註解符號。
- Rust 文件註解與巢狀區塊註解，避免把內容子節點重複算成註解。
- UTF-8、CRLF 的原文、byte range 與位置，以及各語言的語法錯誤。
- Go 長函式中，遠離變更位置的前置註解仍存在於語法樹。

測試中的註解收集器只用來驗證 parser。這些測試不證明 Git diff 與註解的關聯已正確，
也不判斷自然語言註解是否符合程式。Bash 案例不代表完整 POSIX sh 或 zsh 支援。

[擷取測試](tests/extract.rs) 以暫存 Git repo 驗證三種 diff 模式、尚無 HEAD、新增／刪除／
改名、未追蹤檔、衝突路徑、NUL 內容、Unicode／CRLF／特殊檔名、七語言的註解關聯、
函式值、同檔案引用上限、
不完整狀態與退出碼，並確認執行前後 index、狀態與檔案內容不變。

## Claude Code Stop hook

`hooks/claude-stop.sh` 需要 `jq` 與可執行的 `rotter`（預設從 PATH 找，或以 `ROTTER_BIN` 指定）。
每次 Claude 要結束回合時，它在 session 的 `cwd` 執行
`rotter extract --worktree --include-untracked`；若有關聯到註解的變更單元，就回傳
`decision: "block"`，請 agent 依 skill 審查。同一 session 中報告內容未變則不再要求；
由 hook 造成的續跑（`stop_hook_active`）一律放行，所以每回合最多多一次審查。
擷取失敗只以 `systemMessage` 提示，不阻擋。狀態存放在
`${XDG_STATE_HOME:-~/.local/state}/rotter/claude-stop/`（可用 `ROTTER_STATE_DIR` 改）。

安裝（由使用者執行）：

```sh
cargo install --path . --locked
```

在 `~/.claude/settings.json`（或專案的 `.claude/settings.json`）加入：

```json
{
  "hooks": {
    "Stop": [
      { "hooks": [{ "type": "command", "command": "/path/to/rotter/hooks/claude-stop.sh", "timeout": 60 }] }
    ]
  }
}
```

Grok Build 會讀取 Claude Code hooks；hook 也接受 Grok 的 `stopHookActive` 欄位，
但尚未在 Grok 實測。其他限制見 [階段計畫](PLAN.md)。相關範圍與進度見 [階段計畫](PLAN.md)；套件來源見
[parser 依賴查核](docs/parser-dependencies.md)。原始研究保留在
[2026-09-17 交接](HANDOFF-2026-09-17.md)。
