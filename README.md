# Rotter

針對 Git diff 檢查程式註解的 Rust 專案。目前是 POC：`rotter extract` 擷取變更單元與相關註解，
輸出 JSON；語意判斷交給 coding agent 依 [共用 skill](skills/rotter-comment-review/SKILL.md) 執行。
skill 以 `rotter --skill` 隨 binary 發佈；`rotter integration install claude` 註冊 Claude Code Stop hook。

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

沒有副檔名也沒有 shebang 的檔案（例如被 source 的 shell helper）預設不在範圍內，可用
`--lang <glob>=<language>` 指定（可重複，第一個符合者生效，優先於副檔名與 shebang）：

```sh
rotter extract --full --lang '.mise/tasks/lib/*=bash' --lang '**/lib=bash' -- .mise/tasks
```

glob 以 repo 根目錄為基準，`*`、`?` 不跨目錄，`**` 可跨目錄。語言：go、lua、nix、bash、
sh（以 Bash grammar 解析）、yaml、toml、rust，以及設定檔中已啟用的外部語言（見下節）。
被覆寫的檔案 dialect 標為 `<lang>-by-override`。優先順序：`--lang` → 設定檔 `[overrides]` →
內建副檔名／shebang → 外部語言的 `filenames` → 外部語言的 `extensions`。

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
（`GIT_INDEX_FILE`）讀取，避免 `git diff` refresh 時重寫 `.git/index`。`git` 只從 PATH 中的
絕對路徑項目尋找（相對項目如 `.` 會被略過）。只讀取內建與已啟用語言的檔案內容；其他檔案只列路徑。

Git 最低版本為 2.39.1（`safe.bareRepository` 自 2.38 起才有，CVE-2022-23521 的 `.gitattributes`
溢位在 2.39.1 修正；2.38.3 以上的 2.38 修補版也一律拒絕）。在第一次尋找 repo 之前（包括 CLI 與
hook 一開始的 `rev-parse --show-toplevel`）先在 repo 外執行 `git version`；版本較舊或無法辨識時
CLI 退出碼 2，hook 不阻擋，每個 session 以一則 `systemMessage` 提示。

信任說明（repo 內容不受信任）：hook 會在宿主尚未信任的資料夾中執行，repo 的設定可能要求 git
執行指令。擷取期間的每個 git 呼叫（CLI 與 hook、所有模式）都停用以下由 repo 控制的指令路徑：

- filter driver：先以 `git config --null --name-only --get-regexp '^(filter|hook)\.'` 列出名稱
  （取 `filter.` 與最後一個 `.` 之間的文字，名稱含 `=`、`.` 或大小寫都保留），再對每個名稱以
  `--config-env=filter.<name>.<clean|smudge|process|required>=ROTTER_EMPTY_VALUE`（空值）清空；
  列舉失敗（退出碼不是 0／1）或輸出非 UTF-8 時拒絕擷取。
- hook：`core.hooksPath=/dev/null`；`diff.autoRefreshIndex=false`，git diff 不寫 index，
  `post-index-change` 等 index hook（檔案或 git ≥ 2.54 的設定式 hook）不會觸發；設定式 hook
  另以 `--config-env=hook.<name>.event=ROTTER_EMPTY_VALUE` 清空事件。沒有名稱的 `hook.event`
  鍵會讓擷取被拒絕。
- promisor lazy fetch（會執行 repo 的 `uploadpack`、`sshCommand`、`credential.helper`、
  `alternateRefsCommand` 等）：設定 `GIT_NO_LAZY_FETCH=1`，缺少的物件在該側顯示 `read_error`，
  HEAD 指向缺少的 commit 則是擷取錯誤（HEAD 無法解析才算「尚無 commit」）。git 2.39.1–2.45.0
  不依賴這個變數：repo 有 `extensions.partialClone`、`remote.*.promisor` 或
  `remote.*.partialclonefilter`（不論值，含沒有名稱的 `remote.promisor`）時，在讀取任何物件前
  拒絕（CLI 退出碼 2 並指出 partial clone／promisor；hook 以 `systemMessage` 提示）。macOS
  內建的舊版 Apple Git（例如 2.39.x）因此無法處理 partial clone，請把較新的 git 放在 PATH 前面。
- 子模組：`git diff --ignore-submodules=dirty`，不在子模組中以其自身設定執行 `git status`
  （gitlink commit 的變更仍會列出並略過）。
- 隱含的 bare repo：`safe.bareRepository=explicit`，工作目錄內嵌的 bare repo 不會被採用。

因為 git 不再更新 stat 資訊，rotter 自行略過只有 stat 改變的檔案（worktree／base 模式，狀態 `M`、
after oid 全為 0、前後都是一般檔案且 mode 相同）：以不跟隨 symlink、不阻塞的描述子讀取工作目錄
檔案，把這些 bytes 餵給 `git hash-object --no-filters --stdin` 與 before blob 比對；改名與其他狀態
不略過。比對的是原始 bytes，所以 `core.autocrlf`、`ident`、`working-tree-encoding`、`text eol=…`
或 clean filter 會轉換內容的檔案，被碰過就可能顯示為已修改（雜訊）。

其他限制：repo 設定把 `diff.orderFile`、`core.excludesFile` 或 `include.path` 指向 FIFO 或巨大
檔案時，git 可能卡住或變慢（fail-open 的 DoS，以宿主的 hook timeout 為上限）。repo 的
`core.worktree` 決定 rotter 讀取哪個目錄，該目錄可能在 checkout 之外，列出的檔案會被解析進報告。

暫存檔：`TMPDIR`（未設時為平台預設：macOS 為使用者專屬的 `/var/folders/…/T`，Linux 為 `/tmp`）先經與設定檔相同的信任檢查（逐層解析 symlink，每一層目錄須屬於
使用者或 root，且不可被他人寫入，除非是 root 擁有的 sticky 目錄）。不安全的 TMPDIR、或解析後路徑
含 `:` 者，會在建立任何檔案前被拒絕（CLI 退出碼 2 並指出 TMPDIR；hook 以一則 `systemMessage`
提示）。共用 CI 上群組可寫的 TMPDIR 因此會失敗，請改用只有自己可寫的目錄。暫存目錄權限 0700；
每次 diff 使用自己的 `diff-<n>/` 子目錄，`before`／`after` 各以不跟隨 symlink 的方式新建一次，
Git 回傳後即刪除；`git diff --no-index` 以 `GIT_CEILING_DIRECTORIES=<暫存根目錄>` 執行。
私有 index 副本只從一般檔案複製（`.git/index` 是 FIFO、symlink 或裝置時直接報錯）。

## 設定檔與外部 parser（opt-in）

七種內建語言之外的 Tree-sitter grammar 須由使用者在設定檔啟用，並由使用者自己執行
`rotter parser install` 安裝。除了 `parser install`，rotter 不會下載或編譯任何東西；
`extract` 與 hook 只載入已安裝的 library，未安裝時該檔案狀態為 `parser_not_installed`。

設定檔位置：`$XDG_CONFIG_HOME/rotter/config.toml`（`XDG_CONFIG_HOME` 未設或為相對路徑時用
`$HOME/.config`；`HOME` 也未設時沒有設定檔，外部語言停用）。不會從 repo 或目前目錄讀取；
設定檔位於受檢 repo 內、或信任檢查失敗（見上方規則；檔案本身須為一般檔案、屬於使用者或 root、
不可被群組／他人寫入）時，會提示並停用外部語言。home-manager 連到 `/nix/store` 的 root 擁有
唯讀檔可通過。hook 的環境可能與執行 install 的 shell 不同。

```toml
parse_timeout_seconds = 60        # 每個檔案的解析上限（內建與外部語言皆適用），1..=3600，預設 60
languages = ["python", "dockerfile"]  # 啟用 registry 中的 grammar

[language.lua2]                   # 自訂 grammar：path，或 url＋revision（＋location）
path = "/abs/or/relative-to-config-dir/src"   # 放 parser.c 的目錄
# url = "https://…"; revision = "<40 位 commit id>"; location = "sub/dir"（checkout 內含 src/ 的目錄）
symbol = "tree_sitter_lua"        # ^tree_sitter_[a-z0-9_]+$
extensions = ["lua2"]             # 不可與內建副檔名或其他外部語言重複
filenames = ["Luafile", "*.lua2rc"]   # 檔名或檔名 glob（不可含 /、**，不可只有萬用字元）
units = ["function_declaration"]  # 必填：視為單元的 node kind
functions = ["function_declaration"]
function_values = ["function_definition"]
attributes = []
comments = ["comment"]            # 預設 ["comment"]
directives = { "---@" = "lua_annotation" }   # 註解前綴 → directive 標籤
references = true                 # 是否加入同檔案引用，預設 true

[overrides]
"scripts/*" = "bash"              # glob = 內建或已啟用語言
```

未知欄位與不合法值都是錯誤（CLI 退出碼 2；hook 以一則 `systemMessage` 提示並只用內建語言）。
名稱須符合 `^[a-z][a-z0-9_]*$` 且不是內建名稱；url 須以 `https://` 開頭；revision 須為完整 commit id。

Registry（`rotter parser list` 列出已啟用與可用項目；每項釘在最新 release tag 的 commit）：

| 名稱 | 版本 | 檔案 |
| --- | --- | --- |
| python | v0.25.0 | `.py`、`.pyi` |
| javascript | v0.25.0 | `.js`、`.mjs`、`.cjs`、`.jsx` |
| typescript | v0.23.2 | `.ts`、`.mts`、`.cts` |
| hcl | v1.2.0 | `.hcl`、`.tf`、`.tfvars` |
| dockerfile | v0.2.0 | `Dockerfile`、`Containerfile`、`Dockerfile.*`、`*.dockerfile` |

```sh
rotter parser list
rotter parser install              # 安裝全部已啟用的外部 grammar
rotter parser install python       # 或指定名稱
```

安裝：git grammar 以 `git fetch --depth 1` 取得釘選的 commit（樹中不可有 symlink 或 submodule），
本機 `path` grammar 只複製 `parser.c`、`scanner.c`（如有）與 `tree_sitter/*.h`（scanner 引用
其他標頭會編譯失敗；C++ scanner 不支援）。以 `cc` 在快取內的 staging 目錄逐檔編譯並連結，
library 放到 `$XDG_CACHE_HOME/rotter/parsers/<名稱>-<key>.{dylib,so}`（預設 `~/.cache`；
目錄權限 0700，檔名含 revision 或輸入內容的 hash，來源改變就需要重新安裝）。
子程序只拿到 allowlist 環境變數，因此：

- 不讀取 git 設定檔中的 `http.proxy`、`http.sslCAInfo` 等；請改用 `HTTPS_PROXY`、`SSL_CERT_FILE`、
  `GIT_SSL_CAINFO` 等環境變數。
- 需要 `LD_LIBRARY_PATH` 的工具鏈會失敗（fail closed）。
- PATH 上的 ccache／sccache wrapper 可能在 HOME 下寫入自己的快取。

信任說明：啟用 grammar 等於讓它的 C 程式碼在 rotter 程序內、於 hook 看到的每個 repo 中執行。
repo 內容（包含歷史中的舊版本）會交給它的 scanner 處理，所以 scanner 的記憶體安全錯誤就是以
使用者身分執行程式碼；library 的 constructor 在載入時就會執行。更新釘選的 revision 應視為
code review 事件。載入前檢查快取的每一層目錄與 library 的擁有者與權限，但不檢查 macOS
extended ACL（與 OpenSSH 相同）。快取位於受檢 repo 內、或 `parsers/`／library 本身在 repo 內時，
不會載入。

解析時限：每個檔案以 `parse_timeout_seconds` 為上限，逾時狀態為 `parse_timeout`，報告其餘部分照常。
時限只在 parser 步驟之間檢查：卡在 external scanner 自身 C 迴圈的情形不會被中斷，宿主的 hook
timeout 才是最終上限。hook 另有整體軟性期限（見下節）。

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
[repo 指令測試](tests/repo_commands.rs) 以會建立標記檔的 filter、hook、promisor 與子模組 fixture，
經 hook 與 CLI 確認標記從未出現，並各以一般 git 在重建的 fixture 上作為正向對照。

## Agent 整合

skill 已編進 binary，不需另外安裝或維護：`rotter --skill` 印出與此版本相符的審查 skill。

```sh
cargo install --path . --locked
rotter integration install claude     # 在 Claude Code settings.json 加入 Stop hook
rotter integration status
rotter integration uninstall claude
```

`install` 會在 `$CLAUDE_CONFIG_DIR/settings.json`（預設 `~/.claude/settings.json`）的 `hooks.Stop`
加入 `'<rotter 絕對路徑>' hook claude-stop`，`timeout` 為 `max(60, parse_timeout_seconds + 30)`；
**修改 `parse_timeout_seconds` 後須重新執行 `rotter integration install claude`**，`status` 會顯示
`installed (timeout N, expected M)`。修改前把原內容備份成 `settings.json.rotter-bak`（權限 0600），
重複執行不會重複加入，binary 路徑或 timeout 改變時會更新；其他設定與 hooks 不動。
`uninstall` 只移除這一筆，並刪除 `settings.json.rotter-bak`（即使沒有安裝過；只刪一般檔案，
symlink 等會保留並提示）。settings.json 以暫存檔（0600，不跟隨 symlink）原子替換並保留原權限；
settings.json 本身須是一般檔案（symlink、FIFO 會報錯）。

`rotter hook claude-stop` 每次 Claude 要結束回合時，在 session 的 `cwd` 執行
`rotter extract --worktree --include-untracked`；若有關聯到註解的變更單元，就回傳
`decision: "block"`，請 agent 依 `rotter --skill` 審查。同一 session 中報告內容未變則不再要求；
由 hook 造成的續跑（`stop_hook_active`）一律放行，所以每回合最多多一次審查。
擷取失敗只以 `systemMessage` 提示，不阻擋；設定檔提示（HOME 未設、設定被拒、設定錯誤）
同一 session 中，與上一次相同的提示不再重複（只比對最近一次）。狀態存放在
`${XDG_STATE_HOME:-~/.local/state}/rotter/`（可用絕對路徑的 `ROTTER_STATE_DIR` 改；
沒有可用的絕對路徑時不去重）。

hook 的整體軟性期限為 `min(依目前設定計算的 timeout, 已安裝項目的 timeout) − 15 秒`
（只讀使用者層級的 settings.json；project／local／managed 設定不會讀到，估計值可能偏長）。
期限過後尚未開始的檔案直接標 `parse_timeout`。這只涵蓋逐檔的讀取、diff、解析與關聯：
git 子程序或單一慢檔案仍可能超時，宿主 timeout 才是硬上限。

Grok Build 會讀取 Claude Code hooks；hook 也接受 Grok 的 `stopHookActive` 欄位，
但尚未在 Grok 實測。其他限制見 [階段計畫](PLAN.md)。相關範圍與進度見 [階段計畫](PLAN.md)；套件來源見
[parser 依賴查核](docs/parser-dependencies.md)。原始研究保留在
[2026-09-17 交接](HANDOFF-2026-09-17.md)。
