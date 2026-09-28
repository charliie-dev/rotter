# 宿主契約（hosts）

每個宿主（agent）的整合事實都集中在 `src/hosts.rs` 的表格；本頁記錄這些事實的來源、
安裝後的確切格式與擁有權判斷，以及 hook 程序的環境可否被 project 設定注入。新增宿主前，
本頁該宿主的每一格都要有出處；查不到時，提示管道預設為 stderr、續跑判斷預設為「只靠
rotter 的上限」，其他格查不到就標為不支援。已支援：claude、grok、copilot，以及實驗性的 codex、
droid、pi、letta、opencode（rotter 作者未在真實宿主上執行過，`status` 行尾標 `[experimental]`）；
mastracode、devin、cursor、antigravity-cli 查核後標為不支援（見 S2 補遺末尾，`status` 列出原因，
`install` 退出碼 2）。
所有出處皆於 2026-09-29 查閱。

## 宿主表

| 項目 | claude | grok |
|---|---|---|
| 目錄 | `$CLAUDE_CONFIG_DIR`（須為絕對路徑）→ `~/.claude` | `$GROK_HOME`（須為絕對路徑）→ `~/.grok` |
| 安裝方式 | 合併一筆到 `settings.json`（MergeJson） | rotter 自有的 `hooks/rotter.json`（OwnedJson） |
| 事件 | `Stop` | `Stop`（只處理 `reason` 為 `end_turn` 或不存在） |
| session 欄位 | `session_id`，其次 `sessionId` | 同左（Grok 送 `sessionId`） |
| cwd 欄位 | `cwd`，沒有時用 hook 程序自己的目錄 | `cwd`，其次 `workspaceRoot`，都沒有時不做事 |
| 續跑判斷 | `stop_hook_active` 或 `stopHookActive` 為 `true` | 同左 |
| 要求續跑的輸出 | `{"decision":"block","reason":…}` | 同左 |
| 提示管道 | `systemMessage`（每個 session 只提示一次） | stderr（Grok 不顯示，每次都寫） |
| 預設 timeout | 文件為 600 秒（`timeout` 欄位，單位秒）；rotter 保守地以 60 秒估算 | 600 秒（同左） |
| 指令執行方式 | 經 shell；`'<exe>' hook claude-stop \|\| true` | 經 shell，Grok 會展開 `$VAR`；`'<exe>' hook grok-stop \|\| true` |
| 載入 glob | 固定檔名 `settings.json` | `hooks/*.json`（暫存檔 `rotter.json.rotter-tmp` 不符合） |
| 環境可被 project 注入 | 是，屬宿主層級（見下） | 是，屬宿主層級（見下） |
| env-injectable 處理 | 否 | 否 |

`rotter hook claude` 與 `rotter hook claude-stop` 是同一個宿主（`grok`／`grok-stop` 亦同）：
計數器、去重欄位與狀態目錄都以宿主 id 決定，狀態目錄沿用第一版的名稱（`claude-stop/`、
`claude-stop-notes/`、`claude-stop-errors/`、`claude-stop-count/`，grok 為 `grok-stop…`），
所以 S0 寫下的狀態繼續有效。install 一律寫入 `hook claude-stop`／`hook grok-stop`，S0 安裝的
檔案在 S1 仍是 `installed (current)`，不需遷移。

## 擁有權

MergeJson（claude 的 `settings.json`）：一筆項目屬於 rotter，當且僅當它的 `command` 恰為
`'<絕對路徑>' hook claude-stop || true`，路徑以 rotter 寫入的形式引用（`'` 寫成 `'\''`），
逐字元還原後必須再引用成同一字串。

- 路徑是目前的 binary：current（timeout 相同時為 `installed (current)`）；
- 同上但沒有 ` || true`（S0 之前的形式）：mismatch，`status` 顯示 `older command`，install 改寫；
- 同樣形狀、另一個以 `/rotter` 結尾的絕對路徑：另一個 rotter binary，`status` 顯示
  `installed for another binary`，install 以一筆目前的項目取代；
- 其他一律是外來項目（例如 `'/x/rotter-proxy' hook codex`、`'/opt/rotter-dev' hook claude-stop || true`），
  install 與 uninstall 都不碰。

合併檔案本身須是一般檔案、屬於使用者、群組／他人不可寫（否則拒絕並提示 `chmod go-w`）；目錄必須
已存在並通過信任檢查，不會被建立。改寫前以拒絕重複 key 的嚴格解析比對 serde_json 重新序列化的
結果（以 JSON 值比較），不同就拒絕，避免悄悄合併其他工具的重複 key；rename 前重新讀取並逐位元組
比對，檔案在讀取後被改過就放棄寫入（重讀與 rename 之間仍有很小的空窗）。uninstall 只移除 rotter
自己清空的 group，其他工具原本就空的 group 保留。

OwnedJson（grok 的 `hooks/rotter.json`）：內容恰為
`{"hooks":{"Stop":[{"hooks":[{"type":"command","command":C,"timeout":N}]}]}}`，C 為
`'<絕對路徑>' hook grok-stop || true`、N 為正整數；其他內容都不是 rotter 的檔案。

兩種方式在寫入前都拒絕位於 git work tree 內的宿主目錄（見下方「install 的 work tree 檢查」）。

## project 設定能否注入 hook 環境

claude：專案的 `.claude/settings.json` 可以用 `env` 設定 session 的環境變數，這些變數也會傳給
hook 程序；但只有在使用者信任該 workspace 之後才會載入，而此時專案的 hooks 本來就能執行任意指令。
因此這屬於宿主層級的暴露，不另做 env-injectable 處理：PATH 由下方的 git 解析規則處理，HOME、
XDG_*、TMPDIR 等目錄仍經 `resolve_trusted` 的逐層信任檢查。來源（2026-09-29 重新查閱）：
<https://code.claude.com/docs/en/hooks.md>（Stop 輸入 `session_id`、`cwd`、`stop_hook_active`；
`{"decision":"block","reason"}`；`systemMessage` 為通用欄位；shell form 以 `sh -c` 執行並展開變數；
`timeout` 單位秒，command hook 預設 600）、<https://code.claude.com/docs/en/settings.md>
（`CLAUDE_CONFIG_DIR` 取代 `~/.claude`）、<https://code.claude.com/docs/en/env-vars.md>（settings
的 `env` 區塊；project／local 設定不能設 `CLAUDE_CONFIG_DIR`）。

其他宿主也會執行 `~/.claude/settings.json` 裡的 hook：Grok（見下）、Devin CLI（預設讀取
`~/.claude/settings.json`）與 Cursor（third-party hooks）。在這些宿主下 rotter 的 claude 項目一樣
受迴圈上限與共用去重約束；Cursor 會把輸入以 here-document 接在指令後（見下方 cursor），此時
`|| true` 之前的 rotter 收不到輸入，只會靜默結束。

grok：hook 程序的環境是 Grok 程序本身的環境加上 handler 的 `env`（rotter 的文件沒有 `env`，嚴格
格式也不接受）。專案的 `.grok/hooks/*.json` 與 Claude 相容的 `.claude/settings.json` 都需要 folder
trust 才會執行。`session.load_envrc` 的說明是把 `.envrc` 注入 bash 工具；是否也進入 hook 程序文件
未寫明，因此比照 claude 視為宿主層級的暴露。來源：`$GROK_HOME/docs/user-guide/10-hooks.md`
（Hook Locations、Environment Variables）、`05-configuration.md`、`26-config-reference.md`
（`session.load_envrc`）。2026-09-29 以本機 grok 1.0.41 附帶的同一批文件重新核對：`stopHookActive`、
`reason` 為 `end_turn`／`channel_closed`／`shutdown`、`Stop` 預設 600 秒、專案 hook 需要 folder
trust，皆與上表相符。

## git 的選擇（所有宿主與模式）

每次執行 git 之前：

1. 以 realpath 取得 cwd 的實體路徑（失敗時 hook 靜默、CLI 退出碼 2），往上找最上層含 `.git` 項目
   （檔案或目錄）的祖先。排除的樹是實體 cwd 與該祖先，以 `(st_dev, st_ino)` 記錄，symlink、
   firmlink、大小寫不同的拼法都指向同一個 identity。hook 模式下 cwd 以上完全沒有 `.git` 時，
   不執行任何 git。
2. PATH 項目須為絕對路徑、通過 `resolve_trusted`、解析後是目錄、不含 `:`，而且它與每一層祖先都
   不是排除的樹。
3. 候選 `git` 須通過 `trusted_file`，以解析後的實際路徑為準：檔名須是 `git`（mise／asdf 等 shim
   的符號連結會被拒絕）、所在目錄同樣不在排除的樹內；以 `open_regular` 開啟，由該 descriptor 的
   fstat 判斷屬於使用者或 root、群組／他人不可寫、可執行，再讀前幾個位元組：macOS 只接受 Mach-O
   與 `nfat_arch` 在 1..=20 的 universal binary（Java class 檔的同一欄是版本號 ≥ 45），Linux 只
   接受 ELF。`#!` 腳本、讀不到、少於 4 個位元組的候選都略過並繼續往下找；rotter 從不執行候選來
   判斷它。
4. 執行的就是檢查過的解析後路徑。私有的 scratch 目錄（0700）在 `git version` 之前建立，作為每個
   git 子程序的 TMPDIR。`toplevel` 回報的頂層若包含選中的 git 或子程序的 PATH 項目（GIT_DIR、
   `core.worktree`、gitfile 讓 work tree 不在 `.git` 搜尋找到的位置時），就停止（hook 靜默、CLI
   退出碼 2）。

找不到可用的 git 時，hook 靜默，`rotter extract` 退出碼 2，訊息列出被略過的候選與原因；
`integration status` 的 `git:` 行也會顯示。

殘留風險：會依 cwd 挑選程式的原生 dispatcher（例如 proto 的原生 shim）仍會通過；Apple 的
`/usr/bin/git` 經 xcode-select 選擇實際的 git，hook 模式不傳 `DEVELOPER_DIR`；linked worktree 的
主 checkout 不在排除範圍內；`~/.git` 這類 dotfiles repo 會排除 home 下所有 PATH 項目，可能因此
找不到 git（hook 靜默，`rotter extract` 退出碼 2 並指出被排除的 repo）。

## hook 模式的 git 環境

hook 中的每個 git 子程序先 `env_clear()`，只設 PATH（上面篩選過的項目）、HOME、`LANG=C`、
`LC_ALL=C`、TMPDIR（私有 scratch 目錄，不是 temp root 本身），repo 呼叫另加 `GIT_OPTIONAL_LOCKS=0`、
`GIT_NO_LAZY_FETCH=1`、`ROTTER_EMPTY_VALUE=`，需要時加 `GIT_INDEX_FILE`／`GIT_CEILING_DIRECTORIES`；
cwd 為 `/`（每個呼叫都以 `-C` 指定目錄）。不傳 loader 變數（`DYLD_*`、`LD_*`）、`DEVELOPER_DIR`、
`SDKROOT`、`TOOLCHAINS`、XDG_* 或任何繼承的 GIT_*。沒有 XDG_CONFIG_HOME 時，git 讀
`$HOME/.config/git/config`。CLI 的 `extract` 保留使用者自己的環境，只把 TMPDIR 換成 scratch 目錄。

## install 的 work tree 檢查

宿主目錄在 git work tree 內時，checkout、pull 或 commit 都可能改變或公開 hook 檔案，忽略該檔案
也不會讓它離開 work tree，所以 install 拒絕，並建議把宿主目錄移出 work tree 或把環境變數指到別處
（`~/.config` 常是 dotfiles repo）。先做不需要 git 的實體 `.git` 搜尋：上層沒有 `.git` 時直接
安裝、不執行 git。有 `.git` 時以同一套 git 選擇、同樣隔離的環境（沒有繼承的 GIT_*，所以父程序的
`GIT_CEILING_DIRECTORIES`、`GIT_DIR`、`GIT_WORK_TREE` 藏不住 work tree）從 `/` 執行
`rev-parse --show-toplevel`，只有明確的 "not a git repository" 才算在外面；找不到 git、dubious
ownership、逾時（10 秒）或非 UTF-8 輸出都拒絕安裝。

## 迴圈上限

每個宿主、每個 session 最多連續要求 2 次審查；下一個本來要要求的 Stop 改為靜默，這個靜默的 Stop
與任何沒有要求的 Stop 一樣把計數歸零。計數存在 `<state>/<hook>-count/<session>`：在經
`resolve_trusted` 的狀態目錄下以 O_NOFOLLOW 開啟、`flock(2)` 上鎖，在同一個 descriptor 上原地
改寫（不 rename、不 unlink），所以並行的 Stop 會依序讀寫、不會遺失計數。上限一律 fail closed：
沒有可用的狀態目錄、計數讀不到或內容損壞、寫不進去，或 session id 缺少、為空、`.`、`..`、超過
255 位元組時，都不要求審查。沒有「新的使用者回合」訊號可用（rotter 注入的提示看起來也像使用者
訊息）。Grok 的 Claude 相容性下，原生 grok 與相容的 claude handler 各有自己的計數器，而原生
handler 會略過 `stopHookActive` 的 Stop，所以一串續跑最多約 3 次要求（接受）。

## env-injectable 宿主

`Sources` 是 hook 路徑上 home、config、state、cache、temp 目錄與 PATH 的唯一來源。
env-injectable 的宿主改用 `Sources::injectable`：HOME 取自密碼資料庫（`getpwuid_r`，`pw_dir` 須為
絕對路徑，否則沒有基底、hook 靜默），config／state／cache 用 HOME 下的預設位置，temp root 固定為
`/tmp`（仍經 `resolve_trusted` 與私有 scratch 檢查；不用 Darwin 的
`confstr(_CS_DARWIN_USER_TEMP_DIR)`，它失敗時會回頭讀 TMPDIR），行程環境中的 HOME、XDG_*、
ROTTER_* 與 TMPDIR 一律忽略。沒有任何環境變數、參數或檔案能切換這個行為。

## S2 宿主契約補遺

每格的出處列在各節末尾；「經 shell」指宿主把 `command` 字串交給 shell，因此 rotter 寫入
`'<exe>' hook <host> || true`（binary 路徑不可含 `$`、`` ` ``、NUL、換行）。三個已出貨的宿主
都讀得到 project 層級的 hook 設定並執行其中的指令，所以 project 能影響 hook 環境時也已經能直接
執行指令，屬於宿主層級的暴露，不做 env-injectable 處理（PATH 仍由 git 的選擇規則處理）。

### codex（實驗性）

| 項目 | 內容 |
|---|---|
| 目錄 | `$CODEX_HOME`（rotter 只用絕對路徑；Codex 本身也接受相對路徑並 canonicalize）→ `~/.codex` |
| 安裝方式 | MergeJson：`hooks.json`，`{"hooks":{"Stop":[{"hooks":[H]}]}}`，H 為 `{"type":"command","command":C,"timeout":N}` |
| 事件 | `Stop`（不支援 matcher） |
| 輸入 | `session_id`、`cwd`（一定存在，也是 hook 程序的工作目錄）、`stop_hook_active`、`turn_id`、`transcript_path`（不讀）、`last_assistant_message`（不讀）；沒有結束原因欄位 |
| 續跑判斷 | `stop_hook_active` 為 `true` |
| 要求續跑 | `{"decision":"block","reason":…}`；退出碼 2 也會續跑，其他非 0 只記為失敗 |
| 提示管道 | `systemMessage`（Stop 支援，顯示為警告） |
| timeout | `timeout`，單位秒，預設 600 |
| 指令執行 | 經 shell：`$SHELL -lc <command>`（`$SHELL` 取自 Codex 的環境，沒有時 `/bin/sh`；設定可指定 shell）|
| 環境 | Codex 程序自己的環境快照（`std::env::vars_os`），不是 project 設定；project 的 `.codex/config.toml` 只能換 shell，而 project hook 本來就能執行指令：宿主層級 |
| 載入 | 固定檔名 `hooks.json`（及 `config.toml` 的 `[hooks]`）；`hooks.json.rotter-tmp`／`.rotter-bak` 不會載入 |
| 啟用 | `[features] hooks` 預設開啟（舊名 `codex_hooks`）；非 managed 的 hook 須在 `/hooks` 信任後才執行，信任依 hook 內容的 hash 與「來源＋事件＋group 索引＋handler 索引」記錄 |

擁有權同 claude：`command` 恰為 `'<絕對路徑>' hook codex || true`；`'/x/rotter-proxy' hook codex`
等其他形式是外來項目。因為信任記在索引上，install 在原位置改寫第一筆 rotter 項目、移除其他 rotter
項目，沒有時才在最後附加一個 group；uninstall 只移除 rotter 自己清空的 group（rotter 的 group 通常
在最後，不會使其他 hook 位移）。改寫後的項目 hash 不同，Codex 會要求重新信任。`status` 另外一行
顯示 `[features]` 的狀態，並提醒 `/hooks` 信任（rotter 不檢查信任狀態，也不修改 Codex 設定）。

來源：<https://learn.chatgpt.com/docs/hooks>（原 developers.openai.com/codex/hooks）；openai/codex
commit `5a5a4aa79696a4c8a46dea1c9c04066b22559332`：`codex-rs/utils/home-dir/src/lib.rs`
（`CODEX_HOME`）、`codex-rs/hooks/src/engine/discovery.rs`（`hooks.json`、預設 600、trust key 與
hash）、`codex-rs/hooks/src/engine/command_runner.rs`（`-lc`、環境快照）、
`codex-rs/hooks/src/registry.rs`、`codex-rs/hooks/src/events/stop.rs`（輸入欄位、退出碼 2）、
`codex-rs/config/src/hook_config.rs`（`timeout` 欄位名、`HooksFile` 拒絕未知的頂層 key）、
`codex-rs/features/src/lib.rs`（`hooks` 預設開啟）。

### copilot

| 項目 | 內容 |
|---|---|
| 目錄 | `$COPILOT_HOME`（取代整個 `~/.copilot`）→ `~/.copilot` |
| 安裝方式 | OwnedJson：`hooks/rotter.json`，內容恰為 `{"version":1,"hooks":{"agentStop":[{"type":"command","bash":C,"timeoutSec":N}]}}` |
| 事件 | `agentStop`（camelCase 事件，輸入為 camelCase） |
| 輸入 | `sessionId`、`cwd`、`stopReason`（`end_turn`）、`stop_hook_active`、`timestamp`、`transcriptPath`（不讀） |
| 續跑判斷 | `stop_hook_active` 為 `true`；`stopReason` 存在且不是 `end_turn` 時不做事 |
| 要求續跑 | `{"decision":"block","reason":…}`（「block 以 reason 為提示再跑一輪」）；退出碼 2 只是警告，其他非 0 fail-open |
| 提示管道 | stderr（預設；文件沒有 Stop 的使用者訊息欄位） |
| timeout | `timeoutSec`，單位秒，預設 30（別名 `timeout`，`timeoutSec` 優先）；rotter 明寫公式值 |
| 指令執行 | `bash` 欄位以 bash 執行（「Bash commands execute as shell scripts」），會展開 `$VAR`；不用 `exec`/`args` 形式 |
| 環境 | 文件沒有 project 設定可改 hook 環境的機制（handler 的 `env` 只影響該 handler）；repository 的 `.github/hooks/*.json` 本身就能執行指令：宿主層級 |
| 載入 | `hooks/*.json`（使用者層級）；`rotter.json.rotter-tmp` 不以 `.json` 結尾，不會載入 |

擁有權同 grok 的嚴格格式（多了 `version: 1`、handler 平鋪而非 group、指令鍵為 `bash`、timeout 鍵為
`timeoutSec`）；其他內容一律不是 rotter 的檔案，install／uninstall 都不碰。

來源：<https://docs.github.com/en/copilot/reference/hooks-reference>、
<https://docs.github.com/en/copilot/how-tos/copilot-cli/customize-copilot/use-hooks>、
<https://docs.github.com/en/copilot/reference/copilot-cli-reference/cli-config-dir-reference>
（`COPILOT_HOME`）。Copilot CLI 為封閉原始碼（npm `@github/copilot` 1.0.89 只含原生 binary），
執行方式以文件為準。

### droid（實驗性）

| 項目 | 內容 |
|---|---|
| 目錄 | 沒有文件記載的環境變數 → `~/.factory` |
| 安裝方式 | MergeJson：`hooks.json`，事件直接在頂層：`{"Stop":[{"hooks":[H]}]}`，H 為 `{"type":"command","command":C,"timeout":N}` |
| 事件 | `Stop` |
| 輸入 | `session_id`、`cwd`、`stop_hook_active`、`transcript_path`（不讀）、`permission_mode`、`hook_event_name`；沒有結束原因欄位 |
| 續跑判斷 | `stop_hook_active` 為 `true` |
| 要求續跑 | `{"decision":"block","reason":…}`；退出碼 2 也會把 stderr 交給 Droid，其他非 0 不阻擋 |
| 提示管道 | stderr（預設；文件沒有 `systemMessage`） |
| timeout | `timeout`，單位秒，預設 60 |
| 指令執行 | 經 shell（「Hooks run as shell commands」，範例使用 `"$FACTORY_PROJECT_DIR"`） |
| 環境 | 文件沒有 settings 設定環境變數的機制；project 的 `.factory/hooks.json` 本身就能執行指令：宿主層級 |
| 載入 | 固定檔名 `hooks.json`；`hooks.json.rotter-tmp`／`.rotter-bak` 不會載入 |

`hooks.json` 不存在時 Droid 改讀同層 `settings.json` 的 `hooks`（舊格式，`settings.local.json` 疊加
其上）。因此 `hooks.json` 不存在時，install 先讀 `~/.factory/settings.json` 與
`settings.local.json`：任一含非空的 `hooks`，或無法以 JSON 解析（例如含註解），就拒絕建立
`hooks.json`，以免悄悄停用那些 hook（Droid 的 `/hooks` 下次存檔時會自行搬移）。Droid 啟動時快照
hooks，之後的外部修改只會警告，須在 `/hooks` 檢視。擁有權同 claude（`hook droid`）。

來源：<https://docs.factory.ai/reference/hooks-reference>、
<https://docs.factory.ai/cli/configuration/settings>（沒有 `env` 設定、沒有目錄變數）。Droid 為封閉
原始碼。

### 不支援

- mastracode：`~/.mastracode/hooks.json`（目錄只由 `os.homedir()` 決定，沒有變數），以 `/bin/sh -c`
  執行，`timeout` 單位毫秒、預設 10000，輸入有 `session_id`、`cwd`、`stop_reason`，沒有續跑旗標。
  但 Stop 只有在退出碼為 2 時才阻擋，退出碼 0 時 stdout 的 `decision` 被忽略；rotter 的
  `|| true` 形式永遠退出 0，而改成以退出碼 2 續跑會讓舊版或故障的 binary 也能強迫續跑（計畫
  Design 5 排除）。來源：mastra-ai/mastra commit `65a93a2a3b1434d605a6a417cb83d2d58e16bfc0` 的
  `mastracode/sdk/src/hooks/{config,executor,manager,types}.ts`、`docs/src/mastra-code/configuration.mdx`。
- devin：hook 位於 `~/.config/devin/config.json` 的 `hooks`，Stop 輸入只記載 `stop_hook_active`
  與 `session_id`，輸出 `{"decision":"block","reason"}`，退出碼 2 阻擋；但指令是否經 shell、是否
  展開 `$VAR`、預設 timeout、hook 的工作目錄與輸入是否含 `cwd`、`XDG_CONFIG_HOME` 是否改變位置都
  沒有記載（封閉原始碼）。來源：<https://docs.devin.ai/cli/extensibility/hooks/overview>、
  <https://docs.devin.ai/cli/extensibility/hooks/lifecycle-hooks>、
  <https://docs.devin.ai/cli/reference/configuration/read-config-from>。
- cursor：使用者 hook 在 `~/.cursor/hooks.json`（`CURSOR_CONFIG_DIR` 只移動 CLI 的
  `cli-config.json`，不是 hooks），`stop` 輸入有 `conversation_id`、`session_id`、`status`、
  `loop_count`、`workspace_roots`（沒有 `cwd`），輸出 `{"followup_message":…}`（只在
  `status` 為 `completed` 時採用，受 `loop_limit` 限制）。但 cursor-agent（lab 2026.09.26-dd393fe）
  預設的 `argv_heredoc` 傳輸把輸入寫成 `<command> <<'CURSOR_HOOK_EOF'`：here-document 只接到
  `|| true` 後面的 `true`，rotter 收不到輸入；IDE 的執行方式文件未記載。另外 project 的
  `sessionStart` hook 可用 `env` 輸出替之後所有 hook 設環境變數。需要計畫改用例如
  `{ '<exe>' hook cursor || true; }` 的指令形式與 env-injectable 處理後才能支援。來源：
  <https://cursor.com/docs/agent/hooks>、<https://cursor.com/docs/cli/reference/configuration>、
  `https://downloads.cursor.com/lab/2026.09.26-dd393fe/darwin/arm64/agent-cli-package.tar.gz` 的
  `dist-package/190.index.js`（`executeCommandScript`、`buildHookEnvironment`）。
- antigravity-cli：`~/.gemini/config/hooks.json`，頂層是具名的 hook，Stop 為平鋪的 handler 陣列，
  `timeout` 單位秒、預設 30，輸入有 `conversationId`、`workspacePaths`、`terminationReason`、
  `fullyIdle`，輸出 `{"decision":"continue","reason"}`；但指令的執行方式（shell 與否）、退出碼的
  意義與目錄變數都沒有記載（封閉原始碼）。來源：<https://antigravity.google/docs/hooks/>。

## S3 宿主契約補遺（shim 宿主）

pi、letta 與 opencode 沒有指令型 hook，只在自己的程序內載入程式碼，所以 rotter 安裝的是 shim
（OwnedShim）：由內嵌範本（`src/shims/pi-v1.ts`、`src/shims/letta-v1.js`、`src/shims/opencode-v1.js`）
產生、完全屬於 rotter 的檔案。三者的共同規則：

- 範本只有兩個變數，各出現一次：`const EXE = <JSON 字串字面值>;` 與 `const TIMEOUT = <正整數>;`。
  binary 路徑須為絕對路徑的 UTF-8，不可含控制字元、U+2028、U+2029、`$`、`` ` ``（另有 install 的
  binary 信任檢查），只放在那個字面值裡，不出現在註解或 template literal。timeout 為
  `max(60, parse_timeout_seconds + 30)`；pi 與 letta 會等待 handler，上限 120 秒，opencode 不等待
  （見下），不設上限。
- 擁有權：從檔案解出兩個字面值，重新驗證並重新產生，必須與 rotter 出過的某個範本版本（全部內嵌，
  目前只有 v1）逐位元組相同；golden 測試（`tests/fixtures/shim/*-v1.golden.*`）防止已出貨的範本
  被原地修改。其他內容一律是外來程式碼：install／uninstall 退出碼 2 並保留原檔，`status` 顯示
  `foreign code at rotter's path, auto-loaded by <宿主>`。檔案須是一般檔案、屬於使用者、群組／他人
  不可寫；寫入經 `<file>.rotter-tmp`（0600、O_NOFOLLOW），再 rename 成 0600 的 shim。
- 執行：`child_process.spawn(EXE, ["hook", HOST, "--timeout", String(TIMEOUT)], {shell:false,
  detached:true, env:{PATH, LANG}, stdio:["pipe","pipe","ignore"]})`；子程序與 stdin／stdout 都有
  `error` listener，每個 callback 與 kill 各自包在 try/catch，結果只 settle 一次；timeout 到時（或
  stdout 超過 64 KiB）以 SIGKILL 殺掉整個 process group（只在尚未看到 `exit` 時），timer 在 settle 時
  清除並 `unref`。只有 stdout 能解析成 JSON 且 `continue` 為字串時才採用。不使用 `exec`、`execSync`、
  `spawnSync`、`Bun.$`，也不把 `process.env` 傳給 rotter。
- stdin 只有 `{"session_id":…,"cwd":…}`；rotter 的回覆為 `{"continue":<reason>}`，提示寫到 stderr
  （shim 忽略）。shim 另有記憶體中的每 session 計數（同樣最多連續 2 次，被上限靜默的那次歸零）與
  「同時只跑一次」旗標；rotter 端的計數、去重與 fail closed 規則與其他宿主相同。
- `--timeout <n>`（正整數，其他值忽略）只會縮短 rotter 的軟性期限：`min(依設定計算, n) − 15 秒`，
  不會延長。
- env-injectable：shim 只傳 PATH 與 LANG，而且 pi 的 release binary 會讀取工作目錄的 `.env`，所以
  三個宿主都用 `Sources::injectable`（HOME 取自密碼資料庫，XDG_*、ROTTER_*、TMPDIR 忽略）；PATH 仍
  經 git 的選擇規則。沒有任何測試以真實的 stop 執行 `rotter hook pi|letta|opencode`（那會用到真實的 home），
  核心以注入的 `Sources` 在程序內測試。
- 暫存檔 `rotter-review.ts.rotter-tmp`／`rotter-review.js.rotter-tmp` 不符合三個宿主的載入規則（見
  下表）。

shim 測試（`tests/shims.rs`）以 `mise.toml` 固定的引擎執行產生的 shim：pi 在 Node 24.21.0 與
Bun 1.3.14 下各跑一次，letta 用 Node 24.21.0，opencode 用 Bun 1.3.14（都取 mise 安裝目錄下的絕對
路徑，沒有時測試失敗、不略過；log 印出引擎版本，測試也比對它）。搭配 stub 宿主 API
（`tests/fixtures/shim/harness.mjs`，含 `uncaughtException`／`unhandledRejection` 偵測，handler 結束
後再等約 1 秒，然後直接結束、不等 shim 留下的程序）與 stub rotter（`stub.cjs`，以 Node 執行，放在
含空白、引號與非 ASCII 的目錄）。驗證：直接執行（argv 恰為
`hook <host> --timeout 1`）、環境只有 PATH 與 LANG（harness 設了 `LD_PRELOAD`、
`DYLD_INSERT_LIBRARIES`、`DEVELOPER_DIR`、HOME、`GIT_DIR`、`ROTTER_STATE_DIR`）、stdin 內容、三次
連續要求時第三次被壓下並歸零、同時兩次只跑一次、只有字串的 `continue` 被採用，以及 exe 不存在
（ENOENT）、不讀 stdin 就結束（EPIPE）、卡住（1 秒後殺掉整個 process group：孫程序原本會睡 300 秒，比 harness 活得久，
測試結束時必須已不在；拿掉 timer 裡的 `kill()` 時此案例失敗）、
輸出垃圾、輸出超過 64 KiB（卡住或結束）、恰在 timeout 時回覆、宿主 API 丟出例外、非 completed／
end_turn 的回合都靜默且不留下未處理的例外。

### pi（實驗性）

| 項目 | 內容 |
|---|---|
| 目錄 | `$PI_CODING_AGENT_DIR`（Pi 也接受相對路徑與 `~`；rotter 只用絕對路徑）→ `~/.pi/agent` |
| 安裝方式 | OwnedShim：`extensions/rotter-review.ts` |
| 載入 | `extensions/` 下直接的 `*.ts`／`*.js` 檔（或 symlink），以及子目錄的 `index.ts`／`index.js`／`package.json` 的 `pi.extensions`；以 jiti 載入（TypeScript 不需編譯）。`rotter-review.ts.rotter-tmp` 不以 `.ts`／`.js` 結尾，不會載入 |
| 事件 | `agent_before_settle`：`{type, outcome: "completed"\|"aborted"\|"error", entries, continue, context}`，handler 為 `(event, ctx)`，依序 await（沒有 timeout），例外只回報為錯誤 |
| session／cwd | `ctx.sessionManager.getSessionId()`、`ctx.cwd` |
| 續跑判斷 | 沒有續跑旗標：只靠 rotter 的上限與 shim 的計數；shim 只處理 `outcome` 為 `completed` 的回合 |
| 要求續跑 | 回傳 `{entries: [...event.entries, {type:"custom_message", customType:"rotter-review", content, display:true}], continue: true}`（「append entries and request one continuation」） |
| 提示管道 | 預設 stderr（shim 忽略；沒有使用 `ctx.ui.notify`） |
| 引擎 | npm 版：Node ≥ 22.19.0（`engines.node`、README），extension 由 jiti 載入；release binary：Bun 1.3.14 編譯（`build-binaries.yml`），extension 由內嵌的 `jiti/static`（jiti 2.7.0）以 `{moduleCache:false, tryNative:false}` 載入（`loader.ts`、`jiti-static-loader.ts`）。測試在 Node 24.21.0 與 Bun 1.3.14 下都執行；見下方「Bun 下沒有模擬的部分」 |
| project `.env` | release binary 以 `bun build --compile --no-compile-autoload-bunfig` 建置，沒有關閉 `.env` 自動載入（Bun 的預設為開啟），所以工作目錄的 `.env` 會進入 Pi 程序的環境；npm 版（Node）不會。project extension（`.pi/extensions`）需要 project trust 才會載入 |
| env-injectable 處理 | 是 |

Bun 下沒有模擬的部分：harness 以 Bun 自己的 TypeScript 載入器 `import()` shim，不是 Pi 的 jiti；
`bun build --compile` 產生的單一執行檔環境（內嵌模組、`virtualModules`）與 Pi 真正的
`ExtensionAPI` 也沒有模擬（stub 只有 `on` 與 `agent_before_settle` 的欄位）。有執行到的是 Bun 1.3.14
的 `node:child_process`（`detached`、process group 的 SIGKILL、stdin／stdout 事件）、timer 與
Promise 行為，這些是 shim 真正依賴的部分。2026-09-29 另以本機 Bun 快取中的 jiti 2.7.0
`jiti/static`、Pi 的同一組選項手動載入 shim 執行 once／loop／hang／garbage／throwing，結果與上面相同
（沒有加入測試：jiti 不是本專案的依賴）。

來源：<https://github.com/badlogic/pi-mono>（文件連結指向 earendil-works/pi）
commit `fd889a2741891ee45116cb6131052d7fad220886`
（2026-09-28）：`packages/coding-agent/docs/extensions.md`、`docs/configuration.md`、
`docs/environment-variables.md`（`PI_CODING_AGENT_DIR`）、`src/config.ts`（`getAgentDir`）、
`src/core/extensions/loader.ts`（`discoverExtensionsInDir`、jiti）、`src/core/extensions/types.ts`
（`AgentBeforeSettleEvent`、`BoundaryResult`、`ExtensionContext`）、`src/core/extensions/jiti-static-loader.ts`、`src/core/extensions/runner.ts`
（`emitBoundary`）、`src/core/agent-session.ts`（`_runBeforeSettleBoundary`）、`package.json`、
`README.md`、`scripts/build-binaries.sh`、`.github/workflows/build-binaries.yml`；Bun 的
`.env`／bunfig 自動載入預設：<https://bun.com/docs/bundler/executables>（2026-09-29 查閱）。

### letta（實驗性）

| 項目 | 內容 |
|---|---|
| 目錄 | 沒有載入器會讀的變數 → `~/.letta`（`os.homedir()`）；`LETTA_MODS_DIR`／`LETTA_EXTENSIONS_DIR` 只被診斷與 `skills` 子指令使用，mod 載入器固定讀 `~/.letta/mods`（另讀舊的 `~/.letta/extensions`），所以 rotter 不採用這兩個變數 |
| 安裝方式 | OwnedShim：`mods/rotter-review.js` |
| 載入 | `mods/` 下直接的一般檔案（不跟隨 symlink、不以 `.` 開頭），副檔名為 `.js`、`.mjs`、`.ts`、`.tsx`；`rotter-review.js.rotter-tmp` 的副檔名是 `.rotter-tmp`，不會載入。模組須 default export 函式（或 `activate`） |
| 事件 | `letta.events.on("turn_end", (event, ctx) => …)`：`{agentId, conversationId, stopReason, assistantMessage?}`，能力 `letta.capabilities.events.turns`；handler 被 await（沒有 timeout），例外被吞掉 |
| session／cwd | `event.conversationId`，沒有時 `ctx.sessionId`；`ctx.cwd` |
| 續跑判斷 | 沒有續跑旗標：只靠 rotter 的上限與 shim 的計數；shim 只處理 `stopReason` 為 `end_turn` 的回合 |
| 要求續跑 | 回傳 `{continue: "<訊息>"}`（非空字串），Letta 以它作為新的使用者訊息再跑一輪（受 `--max-turns` 約束） |
| 提示管道 | 預設 stderr（shim 忽略） |
| 引擎 | npm 版 `letta.js` 以 `Bun.build({target:"node"})` 打包並加上 `#!/usr/bin/env node`，`engines.node` ≥ 22.19.0（也列 `bun` ≥ 1.3.2）；測試以 Node 24.21.0 執行 |
| project `.env` | Node 不會自動載入 `.env`；settings 的 `env` 只用於少數 Letta 自己的變數，不寫入 `process.env`；Letta 不支援 project mod |
| env-injectable 處理 | 是（shim 只傳 PATH 與 LANG） |

來源：<https://github.com/letta-ai/letta-code> commit `eb5dd97c65fde1168142b5de822f859bc5034059`（2026-09-28）：
`src/mods/paths.ts`、`src/mods/mod-sources.ts`（`listModFiles`、`resolveLocalModSources`）、
`src/mods/file-extensions.ts`、`src/mods/mod-engine.ts`（`getModFactory`、`isTurnEndResultWithContinue`、
`SUPPORTED_MOD_EVENT_NAMES`）、`src/mods/types.ts`（`ModTurnEndEvent`、`ModTurnEndResult`、`ModContext`）、
`src/mods/capabilities.ts`、`src/headless.ts` 與 `src/cli/app/use-conversation-loop.ts`（`turn_end` 與續跑）、
`src/skills/builtin/creating-mods/`（`letta.events.on`、`~/.letta/mods`）、`build.js`、`package.json`。

### opencode（實驗性）

| 項目 | 內容 |
|---|---|
| 目錄 | `$OPENCODE_CONFIG_DIR`（rotter 只用絕對路徑）→ `$XDG_CONFIG_HOME/opencode`（rotter 只用絕對的 `XDG_CONFIG_HOME`）→ `~/.config/opencode`。OpenCode 的全域目錄來自 `xdg-basedir`（`Global.Path.config`，啟動時以 `mkdir -p` 建立）；`OPENCODE_CONFIG_DIR` 設定時是**額外**載入的目錄（排在全域與 `.opencode` 之後），不取代全域目錄，但 OpenCode 自己的 `Global.make()` 也以它為 config 目錄，所以 rotter 依 env 優先規則裝在它下面。之後若不再設定該變數，那裡的 plugin 就不會載入（`status` 以當下的環境判斷）；`xdg-basedir` 接受相對的 `XDG_CONFIG_HOME`，rotter 不接受，此時兩者的位置不同 |
| 安裝方式 | OwnedShim：`plugins/rotter-review.js` |
| 載入 | 每個 config 目錄下 `{plugin,plugins}/*.{ts,js}`（`Glob.scan`，`dot:true`、跟隨 symlink），以檔案 URL 動態 import；`rotter-review.js.rotter-tmp` 不以 `.ts`／`.js` 結尾，不會載入。模組的**每個** export 都必須是 plugin 函式（`getLegacyPlugins`，否則整個模組載入失敗），所以範本只有一個 export：`export const RotterReview = async (input) => ({ event })` |
| 事件 | `session.idle`，`properties` 為 `{sessionID}`，由 `SessionStatus.set` 在狀態變成 idle 時發出；plugin 的 `event` hook 收到 `{event: {id, type, properties}}`，以 `void hook["event"]?.(...)` 呼叫，**不被等待**（因此 handler 絕不 reject，否則成為 OpenCode 的 unhandled rejection） |
| session／cwd | `event.properties.sessionID`；cwd 為 plugin 輸入的 `directory`（該 instance 的目錄；事件只送給 `location.directory` 相同的 instance） |
| 子 session | 以 `client.session.get({path:{id}})` 取得 session，`parentID` 存在（subagent）或取不到時不做事 |
| 續跑判斷 | 沒有續跑旗標：只靠 rotter 的上限與 shim 的計數；注入的提示是新的使用者訊息，shim 不把任何使用者訊息當成歸零的訊號 |
| 要求續跑 | `client.session.promptAsync({path:{id}, body:{parts:[{type:"text", text}]}})`（`POST /session/{id}/prompt_async`，立即返回），在「同時只跑一次」旗標放開之後才呼叫，所以注入的那一輪結束時的 `session.idle` 照常經過計數；失敗被吞掉。這是使用者看得到的新訊息，內容是 rotter 的 `continue`：固定文字、數量、cwd（絕對路徑、無控制字元）與 shell 引用過的 binary 路徑，沒有報告內容或檔名 |
| 提示管道 | 預設 stderr（shim 忽略；沒有使用 `client.app.log`） |
| timeout | shim 內的 timeout；OpenCode 不等待 handler，不設 120 秒上限 |
| 引擎 | release binary 以 `Bun.build({compile})` 建置，macOS／Linux 用 Bun 1.3.14（根目錄 `package.json` 的 `packageManager: bun@1.3.14`）；測試以 Bun 1.3.14 執行 |
| project `.env` | 建置時 `autoloadDotenv:false`、`autoloadBunfig:false`，工作目錄的 `.env` 不會進入 OpenCode 程序；project 的 `.opencode/plugins/` 本來就在同一個程序內執行任意程式碼（宿主層級） |
| env-injectable 處理 | 是（shim 只傳 PATH 與 LANG，rotter 端的 HOME 取自密碼資料庫） |

測試（Bun 1.3.14）另外驗證：模組只有一個函式 export；`session.get` 丟出例外時不執行 rotter；
子 session 不執行 rotter；只有 rotter 要求審查時才呼叫 `promptAsync`（錯誤路徑全部靜默）；
`promptAsync` reject 時被吞掉；stub `promptAsync` 在返回之前就先發出新的使用者訊息事件與下一個
`session.idle` 時，第二個 idle 仍會詢問 rotter（旗標沒有被注入的那一輪佔住，改成佔住時此案例
失敗），三個變動的報告中第三次被壓下（共 3 個 idle、2 次 rotter、2 次 `promptAsync`）。

來源：<https://github.com/sst/opencode> commit `8d05153965bee0a1e46eccffe944dc84d0b0c6f1`
（2026-09-28，2026-09-29 重新查閱）：`packages/opencode/src/config/plugin.ts`（`load` 的 glob）、
`src/config/paths.ts`（`directories`）、`src/config/config.ts`（逐目錄載入 plugin）、
`packages/core/src/global.ts`（`xdg-basedir`、`Global.make`）、`packages/opencode/src/plugin/index.ts`
（`PluginInput`、`getLegacyPlugins`、`void hook["event"]`）、`src/plugin/shared.ts`（`readV1Plugin`）、
`src/session/status.ts`（`Event.Idle`）、`packages/plugin/src/index.ts`（`Hooks.event`）、
`packages/sdk/js/src/gen/sdk.gen.ts`（`Session.get`、`promptAsync`）、
`packages/sdk/js/src/gen/types.gen.ts`（`Session.parentID`、`EventSessionIdle`、
`SessionPromptAsyncData`）、`packages/opencode/script/build.ts`（`autoloadDotenv`）、根目錄
`package.json`、`packages/web/src/content/docs/plugins.mdx`、`config.mdx`（`OPENCODE_CONFIG_DIR`）。
