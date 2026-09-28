# 宿主契約（hosts）

每個宿主（agent）的整合事實都集中在 `src/hosts.rs` 的表格；本頁記錄這些事實的來源、
安裝後的確切格式與擁有權判斷，以及 hook 程序的環境可否被 project 設定注入。新增宿主前，
本頁該宿主的每一格都要有出處；查不到時，提示管道預設為 stderr、續跑判斷預設為「只靠
rotter 的上限」，其他格查不到就標為不支援。目前（S1）只有 claude 與 grok。

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
| 預設 timeout | 60 秒（`timeout` 欄位，單位秒） | 600 秒（同左） |
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
XDG_*、TMPDIR 等目錄仍經 `resolve_trusted` 的逐層信任檢查。來源：Claude Code 文件的 settings
（`env`）與 hooks 章節（<https://docs.claude.com/en/docs/claude-code/settings>、
<https://docs.claude.com/en/docs/claude-code/hooks>；本輪未重新連線查核）。

grok：hook 程序的環境是 Grok 程序本身的環境加上 handler 的 `env`（rotter 的文件沒有 `env`，嚴格
格式也不接受）。專案的 `.grok/hooks/*.json` 與 Claude 相容的 `.claude/settings.json` 都需要 folder
trust 才會執行。`session.load_envrc` 的說明是把 `.envrc` 注入 bash 工具；是否也進入 hook 程序文件
未寫明，因此比照 claude 視為宿主層級的暴露。來源：`$GROK_HOME/docs/user-guide/10-hooks.md`
（Hook Locations、Environment Variables）、`05-configuration.md`、`26-config-reference.md`
（`session.load_envrc`）。

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

## env-injectable 宿主（S1 只有機制）

`Sources` 是 hook 路徑上 home、config、state、cache、temp 目錄與 PATH 的唯一來源。
env-injectable 的宿主改用 `Sources::injectable`：HOME 取自密碼資料庫（`getpwuid_r`，`pw_dir` 須為
絕對路徑，否則沒有基底、hook 靜默），config／state／cache 用 HOME 下的預設位置，temp root 固定為
`/tmp`（仍經 `resolve_trusted` 與私有 scratch 檢查；不用 Darwin 的
`confstr(_CS_DARWIN_USER_TEMP_DIR)`，它失敗時會回頭讀 TMPDIR），行程環境中的 HOME、XDG_*、
ROTTER_* 與 TMPDIR 一律忽略。沒有任何環境變數、參數或檔案能切換這個行為。
