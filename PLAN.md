# 註解檢查 CLI：階段計畫

更新日期：2026-09-28。

本文件記錄目前決定、後續階段與第一版範圍。使用者於 2026-09-18 要求安裝工具、
初始化 Git／Cargo、執行 parser 測試及處理 hooks。本輪已開始工具與 parser 基礎驗證；
完整 Git diff 擷取器及語意驗證 skill 尚未實作，註解審查 hooks 仍缺少這兩項前置功能。

原始研究保留在 [HANDOFF-2026-09-17.md](HANDOFF-2026-09-17.md)，不改寫其歷史內容。
交接之後已確定使用 Rust 實作、由 mise 管理工具，並新增 Rust 為受檢語言。
這些選擇取代交接檔中的舊狀態；原有候選工具研究不代表七語言已通過實測。

## 已確認的決定

| 項目 | 決定 |
| --- | --- |
| 實作語言 | Rust。 |
| 受檢語言 | Go、Lua、Nix、Shell script、YAML、TOML、Rust，共七種；Shell 方言待確認。 |
| 工具管理 | Rust 工具鏈與其他專案開發工具統一由 mise 管理，後續在專案設定中固定版本。 |
| 本次工作 | 安裝工具、初始化 Git／Cargo、驗證七語言 parser，核對 hooks 前置條件。 |

Rust 同時是實作語言和受檢語言，兩者是不同的決定。Rust 1.98.1、rustfmt、Clippy
由 `mise.toml` 管理，套件版本由 `Cargo.toml`／`Cargo.lock` 管理。
本次先使用各語言的獨立 grammar crate，來源見 [parser 依賴查核](docs/parser-dependencies.md)。
目前 Cargo 套件名稱為 `rotter`；正式 CLI 介面、JSON schema 及退出碼仍未定案。

## 階段與步驟

| 步驟 | 工作 | 完成條件 | 狀態 |
| --- | --- | --- | --- |
| 1. 確認第一版範圍 | 確認只讀範圍、CLI 與 agent 分工、diff 語意、Shell 方言及驗收要求。 | 使用者確認下列範圍，沒有會阻擋七語言驗證的範圍歧義。 | 範圍草案已完成，待確認。 |
| 2. 建立最小測試骨架 | 先核對目錄現況；以 mise 管理工具，以 Cargo 鎖定候選解析器依賴，建立測試入口。 | 能執行最小測試，依賴版本與建置需求有紀錄。 | 已完成；格式、Clippy、17 項 parser 測試通過。 |
| 3. 驗證七語言解析與註解關聯 | 逐語言建立變更前後的固定測試檔，核對註解種類、所屬區塊、原文與範圍。 | 測試能重現結果；支援範圍與解析限制清楚，未解決問題沒有被當成通過。 | POC 關聯規則已實作並有七語言測試；已知限制見「2026-09-28 POC」。 |
| 4. 完成 CLI 與 agent 整合 | 依下列順序實作、驗證。 | 各子步驟分別達到驗收要求。 | 4.1、4.2 POC 完成；4.3 做過一輪合成校準；4.4 Claude Code Stop hook 已實作，尚未在宿主實際註冊測試。 |

步驟四依序進行：

1. 實作 Git diff 對應與註解／程式上下文擷取，輸出可核對來源的 JSON。
2. 加上共用驗證 skill，由目前的 coding agent 查證註解是否與程式矛盾。
3. 用真實 diff 校準漏報、誤報與上下文不足的情形。
4. 分別整合 Claude Code、Codex、Grok Build、pi；核對各宿主版本、觸發方式、去重、
   重跑上限與錯誤回饋。這些整合目前都未建立或實測。

若步驟三發現某語言無法滿足要求，先記錄缺口並調整方案或重新確認範圍，
不能默默移除受檢語言，也不能把「套件列出支援」當成驗證結果。

## 步驟一：第一版範圍草案

本節是待確認的建議範圍，尚不是全部實作已獲批准。

### CLI 與 agent 分工

- CLI 只讀取明確選定的變更及相關原始碼，擷取註解與必要上下文；執行檢查不修改受檢專案。
- 收集既有但未變動、新增、修改的相關註解。刪除情境保留舊版註解與程式證據。
- 第一版只承諾變更檔案內的關聯。agent 需要額外定義、imports 或測試時，必須實際追查；
  找不到足夠證據就回報無法確認。
- 共用 skill 由現有 agent 執行語意驗證。問題回報須附註解原文、註解位置、矛盾程式位置與原因；
  程式變更本身不等於註解失真，也不以文字風格或有無同步修改作為判斷標準。
- 特殊註解另外標示，例如 Go 編譯器指示、Lua 型別標註及 ShellCheck 指示。
- 原始碼與註解都當作待檢查資料，不執行其中的指令；遵守使用者及工具既有的讀取限制。

第一版不包含自動修正註解或業務程式、完整跨檔案影響分析、獨立模型服務、資料庫、
MCP server 或大型 agent framework。文件檢查仍交給 docgrad。自動觸發放在步驟四最後驗證。

### Diff 模式的建議語意

每次明確選擇一種模式。下表是行為定義，尚未決定實際 CLI flags。

| 模式 | 變更前 | 變更後 | 涵蓋範圍 |
| --- | --- | --- | --- |
| staged | HEAD。 | Git index（暫存區）。 | 本次已暫存的變更。 |
| working tree | HEAD。 | 工作目錄。 | 追蹤檔案相對 HEAD 的最終差異，包含已暫存與未暫存變更合併後的結果。 |
| 指定 base | 使用者明確指定的 revision。 | 工作目錄。 | 相對指定版本的最終差異；不自動改用共同祖先。 |

- working tree 模式不是單純的「index 對工作目錄」，不只看未暫存變更。
- 未指定模式或 base 無法解析時回報錯誤，不猜測基準、不改用 `HEAD~5` 等替代值。
- 尚無 HEAD 時，staged／working tree 模式以空的變更前版本表示初始狀態，並在輸出中明示。
  指定 base 模式仍要求該版本可解析。
- 未追蹤檔預設不讀取，輸出說明未涵蓋；使用者明確選入後才納入新增檔案。
  Git 忽略規則與敏感檔案讀取限制仍須遵守。
- 新增、修改、純刪除、重新命名都需要測試。重新命名保留前後路徑，不能只剩新檔名。

### 語言與 Shell 方言

七種受檢語言均列入驗證目標。以下是待測的語法情境，不是已核對的 grammar node type。

| 語言 | 優先驗證情境 |
| --- | --- |
| Go | 函式／型別／欄位前的註解、inline／block 註解、編譯器指示。 |
| Lua | 函式、table 欄位、長註解與長字串、型別標註。 |
| Nix | attribute binding、lambda、attrset 與字串內的註解符號。 |
| Shell script | 函式、命令、控制流程、heredoc；依確認的方言建立測試。 |
| YAML | mapping、sequence 與 key-value 附近的註解關聯。 |
| TOML | table／section、key-value 與前置／行尾註解。 |
| Rust | 宣告與函式的一般／文件註解、巢狀區塊註解、raw string 內的註解符號。 |

Shell 建議先驗證 Bash。POSIX sh、zsh 或其他方言是否列入第一版，須由使用者確認。
Bash grammar 不能直接當成其他方言已獲支援的證據。
Rust 第一版以原始檔中的註解為範圍，不承諾巨集展開後的關聯，也不把 `#[doc = ...]`
這類 attribute 自動納入一般註解檢查；若需要，應另行確認範圍。

### 輸出與驗收要求

JSON 至少保留：diff 模式、前後版本識別、檔案前後路徑、語言／方言、註解種類、
註解原文與範圍、相關程式原文與範圍，以及解析／讀取狀態。正式 schema 與退出碼在實作前定案。

- 擷取結果必須綁定同一份來源快照，能用原文核對行範圍及 UTF-8 byte offset。
  來源在處理途中變動時，不能混用不同版本的結果。
- 固定測試包含「只改程式、註解沒改」、只改註解、長函式遠端變更、刪除／搬移／改名，
  以及 Unicode、CRLF 和特殊檔名。
- 字串或 heredoc 裡的註解符號不得誤報為註解；YAML／TOML 使用設定結構的關聯規則。
- 解析錯誤、不支援語法、讀取受限或上下文不足必須明示，不能以空 findings 宣稱通過。
- CLI 擷取成功不代表語意驗證通過；兩個階段的結果分開呈現。
- 在測試中確認檢查前後受檢專案內容不變。

## 步驟一待確認事項

1. 是否採用上述只讀 CLI＋agent skill、變更檔案內關聯，以及三種明確 diff 模式的範圍？
2. Shell 第一版涵蓋 Bash，還是也要求 POSIX sh／zsh？

目前暫以「採用上述範圍、Shell 先做 Bash」作為規劃草案；這兩項尚未獲使用者明確確認。
使用者後續已允許工具安裝、專案初始化與 parser 測試，因此先驗證不依賴正式 CLI 介面的部分。
Bash 的測試結果不代表 POSIX sh／zsh 已受支援；正式 CLI 開始前仍須確認 diff 與方言範圍。

## 依據與目前驗證狀態

- 使用者在本 session 指定 Rust 實作、mise 管理工具，並新增 Rust 為第七種受檢語言。
- `HANDOFF-2026-09-17.md:15–22`：「程式改了，但既有註解完全沒改」；原六語言與四個宿主目標由此延續。
- `HANDOFF-2026-09-17.md:54–67`：「註解擷取 CLI + skill」及「仍需關聯規則」；先驗證擷取，再接語意審查。
- `HANDOFF-2026-09-17.md:83–105`：「不能以空 findings 假裝通過」；來源快照、錯誤狀態及特殊註解須保留。
- `HANDOFF-2026-09-17.md:246–251`：「去重用實際 diff／相關上下文內容 hash」；宿主整合放在核心結果可驗證之後。

2026-09-17 核對時，可見檔案僅有原交接檔，`git rev-parse --show-toplevel` 回報不是 Git 倉庫；
當日只新增本文件。

2026-09-18 已初始化本地 Git 倉庫與 Cargo library 專案，尚未 commit 或設定遠端。
Rust 1.98.1、Cargo 1.98.1、rustfmt 1.9.0、Clippy 0.1.98 的版本命令均已透過 mise 驗證。
首次工具鏈安裝逾時後留下不完整狀態；僅移除本次失敗的 1.98.1 工具鏈，再經 mise 重裝成功。
重裝時使用 rustup 的單執行緒 I/O 診斷選項，根因尚未確認，不把它當成已證實的通用修復。

七個 grammar 與 Tree-sitter Rust binding 已編譯成功。第一個 Go 測試先因缺少
`Language`／`parse` 介面而失敗，再補上解析實作；目前 17 項 integration tests 全部通過。
主流程與獨立驗證均執行 `mise run --skip-tools --no-deps --task-cache off check`，
格式檢查、Clippy 與 17 項測試全部通過，沒有使用 mise 任務快取代替實際執行。

測試包含七語言的變更前後固定檔案、字串與 heredoc 排除、Lua 長註解、Rust 文件／巢狀註解、
七語言的 UTF-8／CRLF 範圍、語法錯誤回報，以及 Go 長函式。位元組範圍和 row／column
由字面預期值核對；註解內容子節點不重複計數。獨立驗證確認這些有限範圍的結果。

驗證限定本機 `aarch64-apple-darwin`、目前固定依賴和測試樣本。Grammar 載入失敗及
無語法樹的錯誤分支有實作，但未刻意觸發；完整語言相容性、跨平台與全功能建置均未驗證。
獨立驗證發現完整離線 metadata 查詢缺少套件快取，主流程已執行 `cargo fetch --locked` 補齊。
之後完整離線 metadata 查詢成功讀取 32 個套件，`cargo test --all-targets --locked --offline`
也通過 17 項測試。這驗證的是目前依賴快取，未測試全新環境的離線安裝或其他平台。

hooks 已核對 Grok 的專案註冊、Stop 回覆、續跑旗標與 session-end 行為；
Claude Code、Codex、pi 的目前版本及實際載入仍未驗證。本輪尚未建立或啟用任何審查 hooks，
也未修改全域宿主設定。parser 測試不能代替 Git diff 註解擷取或語意驗證。

## 2026-09-28 POC

使用者要求「參考 plan 開始建立 POC」並在睡眠期間獨立工作。以下決定原為暫定值，
使用者已於 2026-09-28 確認（Shell 方言改為以 Bash grammar 盡力解析）。

### 決定

- 採用步驟一草案範圍：只讀 CLI＋agent skill、變更檔案內關聯、三種明確 diff 模式。
- CLI：`rotter extract (--staged | --worktree | --base <rev>) [--include-untracked] [-C <dir>]`。
  退出碼 `0` 完整、`1` 有檔案未分析但已輸出 JSON、`2` 錯誤。schema 名為 `rotter.extract.poc/0`。
- Shell：`.bash` 或 bash shebang 為 Bash；`.sh` 無 shebang 視為 Bash 並標 `bash-assumed`；
  `#!/bin/sh`、dash、ash 以 Bash grammar 解析並標 `*-parsed-as-bash`；zsh、ksh 等仍標
  `unsupported_dialect`，使整體結果不完整。
- 同檔案引用（每名稱上限 20）保留。
- 宿主整合只做 Claude Code；使用者表示 Grok Build 會沿用 Claude Code hooks。
- JSON 以約 150 行自寫輸出，不新增 serde 依賴（自主工作期間不新增套件）。
- 共用 skill 放在 `skills/rotter-comment-review/SKILL.md`，未安裝到任何宿主。

### 實作摘要

- `src/git.rs`：以 `git diff --raw -z -M --no-abbrev` 取得檔案清單與 blob id；before 與 staged 的
  after 以 `git cat-file` 讀 blob，working tree／base 的 after 讀磁碟並以 `hash-object --no-filters`
  記錄 blob id。行差異由 `git diff --no-index -U0` 對「實際讀入的同一份內容」計算，避免混用快照。
  不追蹤 symlink、跳過 submodule；只讀取七語言的檔案內容。
- `src/comments.rs`：以變更行找所屬單元（各語言 unit kinds；函式內的小單元併入函式；
  無單元時退回頂層敘述）。註解關聯：`leading`（依行相鄰，可跨 Rust attribute，遇 `//!`、
  shebang 停止）、`inside`、`trailing`、`enclosing_leading`。只改註解時沿著註解找到它說明的單元。
  刪除行以 gap 表示，選出跨越刪除點的單元。同檔案中使用變更名稱的單元以 `reference` 加入，
  每名稱上限 20 個並記錄省略數。相鄰同類行註解合併；directive 另列。

### 驗證

- `mise run check`：fmt、Clippy（`-D warnings`）、47 項測試全數通過（parser 17、擷取 23、hook／整合 3、單元 4）。
- 擷取測試涵蓋步驟一驗收清單：只改程式／只改註解、長函式遠端變更、刪除／新增／改名、
  staged 與 working tree 差異、無 HEAD、base 解析失敗與選項注入、未追蹤檔排除／納入與 gitignore、
  Unicode／CRLF／含空白與非 ASCII 的檔名、語法錯誤／不支援方言、退出碼，以及執行前後
  index、`git status` 與檔案內容不變（含 stat-dirty 檔案）。每份測試輸出都核對
  `text == 快照[bytes]` 與 blob id。另測函式值（Lua／Go／Nix）、NUL 內容、衝突中的路徑。
- 獨立 verifier 兩輪：第一輪找到 worktree／base 模式會重寫 `.git/index`（`git diff <tree>`
  即使 `GIT_OPTIONAL_LOCKS=0` 仍會 refresh）、函式值的 leading 註解遺失；第二輪找到衝突路徑在
  「磁碟內容等於 HEAD」「modify/delete」「已刪除」時漏報。均已修正並加回歸測試：Git 改用
  `GIT_INDEX_FILE` 指向私有 index 副本並關閉 split index；衝突路徑一律列為 `unmerged`；
  含 NUL 的檔案標 `contains_nul`（Tree-sitter 會在 NUL 處截斷）。最後一輪修正後未再請
  verifier 複驗，只有本地測試與手動重現確認。
- 真實檔案校準：從 cargo registry 取 22 個 Rust／TOML／YAML／Bash 檔，自動修改數字與刪除註解，
  全部解析成功、範圍 0 不符；輸出約 150KB。Go／Lua／Nix 無現成真實檔可用，只有合成測試。
- 語意盲測：7 語言各植入失真或仍正確的註解，交給不知答案的 agent 依 skill 審查。
  6 個失真全數找出（含常數在別處被改、刪除 assert 後的 SAFETY 註解、失效的 ShellCheck 指示），
  2 個仍正確的註解沒有誤報，另回報 1 個邊界案例（`saturating_add` 溢位）。依回饋補上
  gap 範例、引用刪除行、低信心邊界案例的寫法。樣本很小，不代表真實誤報／漏報率。

### 已知限制（POC）

- 關聯是語法＋行相鄰的啟發式；與單元隔一空行的區段註解、檔案頂部說明不會列為 `leading`。
- 同檔案引用只比對識別字文字，不解析作用域；YAML／TOML 不做引用。跨檔案影響不在範圍內。
- 單元原文未截斷：Nix 整檔 lambda 參數變更或大型 YAML block scalar 會輸出整段。
- 語法錯誤時輸出 `partial`：仍給能解析的單元並標出與錯誤重疊者；錯誤區域內的關聯可能不準。
  tree-sitter-bash 無法解析 `"${VAR:?訊息 (含括號)}"`，local-env 的 deploy／install 即因此成為 partial。
- 未處理的 Git 狀態：衝突中的 unmerged 項目僅標不完整；未測試 sha256 物件格式 repo。
- split index repo：Git 讀取 split index 時會更新 `.git/sharedindex.*` 的 mtime（內容不變，
  `git status` 也會如此）；除此之外 `.git` 與工作目錄不變。
- 無副檔名檔案：磁碟側只讀前 4096 bytes 判斷 shebang；before 側（Git blob）仍整份讀取。
- 未驗證其他平台；只在 `aarch64-apple-darwin`、Git 2.54.0 執行。

### Full-codebase 模式與 pathspec（2026-09-28 使用者要求）

- `--full`：快照為工作目錄中的追蹤檔案（已從磁碟刪除者略過），`--include-untracked` 可加入未追蹤檔。
  以註解為起點選單元；單元原文超過 80 行截斷。沒有 before 側、沒有 changed 標記。
- pathspec 適用四種模式，相對於執行目錄；不給時等同 `:/`（整個 repo）。
- local-env 實測：240 個檔案、1072 個單元、1212 則註解、約 1.5MB、1.3 秒；執行前後 index 與狀態不變。
  full 模式的語意審查尚未實際跑過，skill 只寫了分批方式。

### 語言覆寫（2026-09-28 使用者要求）

`--lang <glob>=<language>` 讓沒有副檔名、也沒有 shebang 的檔案（如被 source 的 helper）納入分析；
可重複，第一個符合者生效，優先於副檔名與 shebang。local-env `.mise/tasks` 的 12 個 helper 以此解析，
全部 `ok`，共 154 個單元、175 則註解。

### 4.4 Claude Code Stop hook

`rotter hook claude-stop`（原為 bash＋jq 腳本，2026-09-28 改為內建子指令）：在 session `cwd` 執行 `rotter extract --worktree --include-untracked`，
有關聯單元時回傳 `decision: "block"` 並指向 skill；`stop_hook_active`／`stopHookActive` 為真時放行，
同一 session 相同報告（cwd＋報告內容的 sha256）不重複要求；擷取失敗以 `systemMessage` 提示。
`rotter integration install|uninstall|status claude` 管理 `$CLAUDE_CONFIG_DIR/settings.json` 中的
這一筆（備份、冪等、保留其他設定）；skill 以 `rotter --skill` 隨 binary 發佈，仿 herdr 的形式。
`tests/hook.rs` 以假輸入驗證 hook 行為，並以暫存 `CLAUDE_CONFIG_DIR` 驗證安裝與移除。

尚未完成：

- 未在 Claude Code 或 Grok Build 實際註冊與觸發；需要使用者執行 `cargo install` 與
  `rotter integration install claude`（沙箱內無法寫入 Claude 設定）。
- 每回合比較的是 HEAD 對工作目錄的累積差異，長 session 中每次程式變動都會重審全部變更單元。
- Codex、pi 依使用者決定不做。

### 外部 parser、設定檔與解析時限（2026-09-28 使用者要求，計畫 revision 16）

計畫經 plan review、security review（無 P0–P2）與外部 review 通過後分三段實作：

- A（grammar／config／timeout）：`Grammar` 取代封閉的語言列舉作為分析單位；
  `$XDG_CONFIG_HOME/rotter/config.toml`（`parse_timeout_seconds`、`languages`、`[language.<name>]`、
  `[overrides]`），逐層信任解析（`resolve_trusted`），設定檔在 repo 內或不可信時停用外部語言；
  每檔解析時限與 hook 整體軟性期限（`min(計算值, 已安裝 timeout) − 15 s`）；`integration install`
  依設定寫入 `timeout`。
- B（installer／loader／registry）：`rotter parser install|list`；git grammar 釘 commit、
  本機 `path` grammar 逐檔信任檢查；在快取內 staging 目錄以 `cc` 編譯，子程序只拿 allowlist
  環境變數；loader 只在遇到該語言檔案時 lstat 驗證並 dlopen，未安裝為 `parser_not_installed`。
- C（既有暫存路徑與整合修正）：`Scratch` 的暫存根目錄經 `resolve_trusted`，含 `:` 者拒絕；
  每次 diff 使用新的 `diff-<n>/`，輸入以 create_new＋O_NOFOLLOW 建立，`GIT_CEILING_DIRECTORIES`
  限制搜尋；私有 index 只從一般檔案複製（保留 mtime，否則 Git 的 racily-clean 判斷會漏掉同大小的修改）。
  settings.json 原子寫入（0600、fchmod、不跟隨 symlink），只有 install 建立備份，uninstall 刪除
  一般檔案的備份；git 只從 PATH 的絕對路徑項目尋找；hook 指令以 `shell_quote` 引用；設定提示
  每 session 去重；`integration status` 可讀地顯示非整數 timeout；loader 也拒絕位於 repo 內的
  `parsers/` 與 library。

Registry 手動檢查（需要網路，在沙箱外執行：沙箱 proxy 需要 git 設定檔中的設定，而 installer 的
allowlist 環境不讀 git 設定檔）。每項 `rotter parser install <name>` 成功，並以 `extract --full`
在小型樣本檔上回報至少一個帶 leading 註解的單元：

| 名稱 | tag | revision | library 檔名 |
| --- | --- | --- | --- |
| python | v0.25.0 | `293fdc02038ee2bf0e2e206711b69c90ac0d413f` | `python-293fdc02…-6c1d2d18.dylib` |
| javascript | v0.25.0 | `44c892e0be055ac465d5eeddae6d3e194424e7de` | `javascript-44c892e0…-4f8bcb2e.dylib` |
| typescript | v0.23.2 | `f975a621f4e7f532fe322e13c4f79495e0a7b2e7` | `typescript-f975a621…-c8c65251.dylib` |
| hcl | v1.2.0 | `fad991865fee927dd1de5e172fb3f08ac674d914` | `hcl-fad99186…-52bd1e51.dylib` |
| dockerfile | v0.2.0 | `868e44ce378deb68aac902a9db68ff82d2299dd0` | `dockerfile-868e44ce…-0a13bab2.dylib` |

library 檔名由 revision 與 location／symbol 的 FNV-1a hash 算出；各項單元數未記錄在本文件。
新增依賴 `toml`、`libloading` 以 `=` 釘版；`cargo audit` 在此環境不可用，未執行。

已知限制：

- 解析時限只在 parser 步驟之間檢查，卡在 external scanner 自身 C 迴圈時不會中斷；hook 整體期限
  是軟性的（git 子程序、單一慢檔案仍可能超過），宿主 timeout 才是硬上限；只讀使用者層級 settings.json。
- 外部 grammar 的 C 程式在程序內執行，load 時 constructor 即執行；scanner 記憶體安全錯誤等同以使用者
  身分執行程式碼。不做 fd-based dlopen；不檢查 macOS extended ACL。
- git 設定檔中的 proxy／CA 不讀取；需 `LD_LIBRARY_PATH` 的工具鏈失敗；ccache 可能寫入 HOME。
- 群組可寫的 TMPDIR（常見於共用 CI）現在會被拒絕；symlink 或 FIFO 形式的 settings.json 會被拒絕。
  使用者於 2026-09-28 確認維持拒絕 symlink 形式的 settings.json（避免把 dotfile 管理的連結悄悄換成一般檔案）；
  透過 symlink 目錄（如 `~/.claude -> ~/.config/claude`）存取仍可正常運作。
- 只在 `aarch64-apple-darwin` 驗證；O_NOFOLLOW／O_NONBLOCK 常數只定義 macOS 與 Linux x86_64／aarch64。
