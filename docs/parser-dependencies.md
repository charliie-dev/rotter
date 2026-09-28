# Parser 依賴查核

查核日期：2026-09-18。本頁記錄七語言測試採用的固定版本與靜態來源。
執行結果與尚未驗證的範圍記錄在 [PLAN.md](../PLAN.md)；本輪未做完整供應鏈稽核。

## 工具鏈

Rust stable 發佈清單提供 `1.98.1`，發佈日期為 2026-09-03，
`aarch64-apple-darwin` 的 rustc、Cargo、標準函式庫、rustfmt、Clippy 均標為可用。
專案在 `mise.toml` 固定此版本，使用 minimal profile 加上 rustfmt 與 Clippy。

來源：[Rust 1.98.1 發佈清單](https://static.rust-lang.org/dist/channel-rust-1.98.1.toml)，
`[pkg.rustc.target.aarch64-apple-darwin]` 的 `available = true` 等元件項目。
這是官方發佈資訊，不等於本機安裝成功。

## 採用套件與來源

版本與 repository 欄位由 crates.io 查得；revision 由各版 `.crate` 檔中的
`.cargo_vcs_info.json` 核對。以下套件皆標示 MIT 授權；固定測試的結果不代表所有語法均已驗證。

| 用途 | 套件版本 | 來源 revision |
| --- | --- | --- |
| Runtime／Rust binding | `tree-sitter = 0.27.0` | [6070dbf](https://github.com/tree-sitter/tree-sitter/tree/6070dbfefd326bd735e5683eb128cc1b57dad0c0/lib) |
| Go | `tree-sitter-go = 0.25.0` | [1547678](https://github.com/tree-sitter/tree-sitter-go/tree/1547678a9da59885853f5f5cc8a99cc203fa2e2c) |
| Lua | `tree-sitter-lua = 0.5.0` | [10fe005](https://github.com/tree-sitter-grammars/tree-sitter-lua/tree/10fe0054734eec83049514ea2e718b2a56acd0c9) |
| Nix | `tree-sitter-nix = 0.3.0` | [ea1d87f](https://github.com/nix-community/tree-sitter-nix/tree/ea1d87f7996be1329ef6555dcacfa63a69bd55c6) |
| Bash | `tree-sitter-bash = 0.25.1` | [a06c2e4](https://github.com/tree-sitter/tree-sitter-bash/tree/a06c2e4415e9bc0346c6b86d401879ffb44058f7) |
| YAML | `tree-sitter-yaml = 0.7.2` | [7708026](https://github.com/tree-sitter-grammars/tree-sitter-yaml/tree/7708026449bed86239b1cd5bce6e3c34dbca6415) |
| TOML | `tree-sitter-toml-ng = 0.7.0` | [64b5683](https://github.com/tree-sitter-grammars/tree-sitter-toml/tree/64b56832c2cffe41758f28e05c756a3a98d16f41) |
| Rust | `tree-sitter-rust = 0.24.2` | [e2bee85](https://github.com/tree-sitter/tree-sitter-rust/tree/e2bee853694a1d3e0f6ef308fe3674542fec95d7) |

Lua 的此版來源是 `tree-sitter-grammars/tree-sitter-lua`，與原交接列出的候選來源不同。
本次 TOML 套件是 `tree-sitter-toml-ng`，需核對 crate 名稱，不能僅依 repository 名稱推定。

## 已核對的介面與建置需求

- 七個 grammar 的 Rust binding 都提供 `pub const LANGUAGE: LanguageFn`。
  例如 Go 的 [`bindings/rust/lib.rs:30–36`](https://github.com/tree-sitter/tree-sitter-go/blob/1547678a9da59885853f5f5cc8a99cc203fa2e2c/bindings/rust/lib.rs#L30-L36)。
- Runtime 使用 `Parser::set_language` 檢查 grammar ABI，`Parser::parse` 回傳 `Option<Tree>`。
  來源：[`lib/binding_rust/lib.rs:778–784`](https://github.com/tree-sitter/tree-sitter/blob/6070dbfefd326bd735e5683eb128cc1b57dad0c0/lib/binding_rust/lib.rs#L778-L784)
  的 `MIN_COMPATIBLE_LANGUAGE_VERSION..=LANGUAGE_VERSION`，以及
  [`:926–932`](https://github.com/tree-sitter/tree-sitter/blob/6070dbfefd326bd735e5683eb128cc1b57dad0c0/lib/binding_rust/lib.rs#L926-L932)
  的 `pub fn parse`。
- `Point` 的 row、column 為零起算，型別是 `usize`。
  來源：[`lib/binding_rust/lib.rs:101–106`](https://github.com/tree-sitter/tree-sitter/blob/6070dbfefd326bd735e5683eb128cc1b57dad0c0/lib/binding_rust/lib.rs#L101-L106)。
- Runtime 預設功能為 `std`；本次不啟用 `wasm` 或 `bindgen`。
  套件內 `Cargo.toml` 的 `[features]` 指定 `default = ["std"]`，`wasm` 才引用 `wasmtime-c-api`。
- Runtime 與 grammar 會編譯 C 程式。來源包含
  [`lib/binding_rust/build.rs:41–56`](https://github.com/tree-sitter/tree-sitter/blob/6070dbfefd326bd735e5683eb128cc1b57dad0c0/lib/binding_rust/build.rs#L41-L56)
  的 `.file(src_path.join("lib.c"))`、`.compile("tree-sitter")`，以及各 grammar 的
  `bindings/rust/build.rs`。Git 和 C 編譯器／系統 SDK 是宿主前置需求；本機已有 Apple Git 與 Clang，
  本次未另外安裝或取代。Rust 開發工具由 mise 管理。

## 註解節點

各版套件的 `src/node-types.json` 宣告下列節點；測試須再核對實際原文、範圍及語法錯誤。

| 語言 | 完整註解節點 | 注意事項 |
| --- | --- | --- |
| Go、Nix、Bash、YAML、TOML | `comment` | 字串中的標記不得當成註解。 |
| Lua | `comment` | `comment_content` 是內容子節點，不能重複算成另一則註解。 |
| Rust | `line_comment`、`block_comment` | `doc_comment` 與內外文件註解 marker 是子節點，不能重複計數。 |

測試保留 grammar 的原始範圍，沒有強行統一換行處理。在本次單行註解案例中，
Go／Nix／Bash／Rust 的範圍包含尾端 CR，Lua／YAML／TOML 不包含；Rust 的文件單行註解
也會包含測試原文中的 LF。對照 `tests/parsers.rs:166–223` 的原文、byte range 與 Point 斷言。

本階段只驗證 parser 及來源範圍。Git diff 對應、完整註解關聯規則、語意驗證 skill，
以及 Claude Code／Codex／Grok Build／pi 的自動審查 hooks 仍是後續工作。
