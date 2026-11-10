# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## 語言與安全

- 對話與產出一律用**繁體中文（台灣用語）**；程式碼、註解、commit message 沿用 repo 既有的英文。
- 不在對話中貼出 API key、secret 等機密。交易所 API 憑證由 app 自己存（`data/src/config/auth.rs`），不要寫死進程式碼或 fixture。

## 專案與 Git 遠端

Flowsurface：以 [iced](https://github.com/iced-rs/iced) 0.14（wgpu）寫成的原生桌面加密貨幣圖表 app，支援 Binance、Bybit、Hyperliquid、OKX、MEXC。

- `origin` = 個人 fork（`darkstar1227/flowsurface`），`upstream` = `flowsurface-rs/flowsurface`。只 push 到 `origin`；未經使用者要求，不對 upstream 開 PR、也不 push。
- Commit message 沿用 upstream 風格：小寫、祈使句的簡短摘要，必要時用 ` - ` 分隔補充說明，例如 `kline chart: use pointer cursor on armed drawing tool - ...`。**不用** Conventional Commits 前綴。
- Stage 時一律指定明確路徑（`git add -- <paths>`），不要 `git add -A` / `git add .`。

## 指令

Toolchain 固定為 Rust 1.95.0（`rust-toolchain.toml`），edition 2024。Linux 需要 `libasound2-dev`（ALSA）。

```bash
cargo run                       # debug build + 啟動（debug 時 logger 較詳細）
cargo run --release
cargo check --workspace         # 只做型別檢查，最快
```

套件名稱與目錄名不同：`data/` → `flowsurface-data`、`exchange/` → `flowsurface-exchange`、`indicator-engine/` → `flowsurface-indicator-engine`。在其他 crate 裡分別以 `data`、`exchange`、`indicator_engine` 引用。所有 `-p` 參數都要用套件名稱。

## Rust Lint

CI 的 `format.yml`、`lint.yml` 會在 PR 上執行，要求零警告：

```bash
cargo fmt --all                  # 自動格式化（commit 前執行）
cargo fmt --all -- --check       # CI 用的檢查模式
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo clippy -p flowsurface-data --all-targets -- -D warnings   # 只檢查單一 crate，較快
```

- `rustfmt.toml`：`max_width = 100`、edition 2024。不要手動排版，交給 `cargo fmt`。
- `clippy.toml` 放寬了 `too-many-arguments-threshold = 16` 和 `enum-variant-name-threshold = 5`，其餘 lint 一律使用預設，並以 `-D warnings` 視為錯誤。
- 修 clippy 警告時，要真的改寫程式碼，不要加 `#[allow(clippy::...)]` 把警告壓掉；確實需要時，在旁邊寫一行註解說明原因。
- 要先 `cargo fmt`，再跑 clippy；`--all-targets` 會一併檢查測試與 bin 的程式碼。

## Rust Test

依 CI（`lint.yml`）的順序執行：

```bash
cargo test -p flowsurface-indicator-engine --locked   # 語言 parser／runtime／warmup 與 CLI 測試
cargo test -p flowsurface --bin flowsurface --locked  # GUI bin（目前沒有測試模組，主要確認能編譯）
cargo test -p flowsurface-data --lib --locked         # 指標 registry／loader／scripting，會載入 data/indicators/sources 內的全部 .ind
cargo run --locked -p flowsurface-indicator-engine --bin flowsurface-indicator -- \
  test data/indicators/sources --namespace core       # 用合成 K 棒對內建指標做 smoke test
cargo package --workspace --locked --target-dir "$(mktemp -d)"   # 確認各 crate 可以打包（改動 Cargo.toml 或新增檔案時執行）
```

單一測試或部分測試：

```bash
cargo test -p flowsurface-indicator-engine <name_substring>
cargo test -p flowsurface-data --lib <module>::tests::<name> -- --exact --nocapture
cargo test -p flowsurface-indicator-engine -- --list   # 列出所有測試名稱
```

- 測試採用 inline 的 `#[cfg(test)] mod tests`。目前只有 `indicator-engine/src/{lib,warmup}.rs`、`indicator-engine/src/bin/flowsurface-indicator.rs`，以及 `data/src/chart/kline/indicators/`（`registry`、`loader`、`scripting`、`language`、`core/runtime`）有測試。`exchange` 和 GUI 目前沒有測試。
- `indicator-engine/tests/fixtures/*.ind` 是透過 `include_str!` 載入的測試輸入，不是 integration test。新增語言功能時，在此加入 fixture，並在 `lib.rs` 的測試中引用。
- 修改 `data/indicators/sources/` 下的 `.ind`，或修改指標語言時，`flowsurface-data` 的測試和 CLI `test` 兩者都要跑。
- 依改動範圍挑選要跑的測試：只改 `exchange/` 或 `src/` 時，至少跑 `cargo clippy --workspace --all-targets -- -D warnings`，並用 `cargo run` 實際確認行為。
- 輸出很長，執行時在指令中過濾，例如 `2>&1 | tail -40` 或 `| grep -E '^error|^warning|FAILED|panicked|test result'`。

## 架構

Cargo workspace，依賴方向 `flowsurface`（root，GUI）→ `data` → `exchange`；`indicator-engine` 是獨立 crate，由 `data` 和 GUI 使用。

- **`exchange/`**：與 UI 無關的交易所層。`adapter/hub/<venue>/` 是各交易所的 REST 擷取與 WebSocket stream；`adapter/{ws,http,limiter,proxy,client}.rs` 是共用的連線、限流與 proxy 機制。對外輸出統一的 `exchange::Event`，以及 `Ticker`、`Kline`、`Trade`、depth 等型別。
- **`data/`**：可序列化的狀態與與 GUI 無關的運算，包括聚合（`aggr/` 的 time／tick 聚合）、圖表資料模型（`chart/kline`、`chart/heatmap`）、面板模型（`panel/` 的 ladder、time & sales）、設定（`config/` 的 theme、timezone、auth、network）、layout／preset 的持久化，以及指標 catalog／manifest（`chart/kline/indicators/`）。
- **`indicator-engine/`**：`.ind` 指標語言的 parser → HIR → runtime，另附 `flowsurface-indicator` CLI（`new` / `check` / `test` / `catalog`）。語言規格見 `indicator-engine/LANGUAGE.md`，CLI 見 `indicator-engine/README.md`。內建指標放在 `data/indicators/sources/`，namespace 為 `core`。
- **`src/`（GUI）**：iced `daemon` 多視窗 app，採 Elm 架構。`main.rs` 的 `Flowsurface` 負責頂層 `update` / `view` / `subscription`。
  - `layout.rs` + `modal/layout_manager.rs`：多個具名 layout，每個 layout 擁有一個 `screen::dashboard::Dashboard`（pane grid，可 popout 成獨立視窗）。
  - `screen/dashboard/pane/`：`state.rs` 管理各 pane 的 stream 綁定，`content/` 是各種 pane 類型（kline、heatmap、ladder、timesales）的邏輯，`settings/` 是各 pane 的設定。
  - `widget/chart/`：自訂 canvas／shader 圖表 widget（kline 的 drawing、footprint、studies、scene／painting 都在這裡）。
  - `connector/`：歷史資料擷取（`fetcher.rs`）、對 Flowsurface server 的 client、指標 catalog client。

### 資料流

1. `Flowsurface::subscription` 呼叫 `Dashboard::market_subscriptions`，從所有 pane 收集 depth／trade／kline 的 stream spec（排序過，subscription identity 才會穩定，見 `3561056`），再對應成 `exchange` 的 WebSocket subscription，結果以 `Message::MarketWsEvent` 傳回。
2. 頂層 `update` 把事件交給作用中 dashboard 的 `update`，由它分派到對應 pane。擷取到的資料直接在 `Dashboard::update` 內分派，`dashboard::Event` 也就地處理，不再轉發回 main（見 `c276161`）。
3. 歷史 kline、OI、ticker metadata 一律走交易所 REST。歷史成交依 **Settings → Network** 模式而定：Off／Exchange（只限 Binance，使用 data.binance.vision 封存檔並快取在本機磁碟）／Server（`GET /trades.arrow`，Arrow IPC 格式）。
4. 狀態持久化：`data::read_from_file` / `write_json_to_file` 讀寫資料目錄下的 `saved-state.json`（`data::config::state::State`）。資料目錄預設為平台資料夾下的 `flowsurface/`，可用 `FLOWSURFACE_DATA_PATH` 覆寫；已安裝指標的目錄可用 `FLOWSURFACE_INDICATORS_PATH` 覆寫。啟動時會先初始化指標 catalog，並在背景清理過期的市場資料快取。

### 注意事項

- 修改任何會序列化進 `saved-state.json` 的型別（layout、pane 設定、theme、preset），會影響既有使用者的存檔，需維持向後相容（serde default／rename）。
- Windows 上停用多視窗（winit redraw 問題）；改動 popout／window 邏輯時要考慮這個平台分支。
- 授權：app、`data` 與內建指標是 GPL-3.0-or-later；`indicator-engine` 使用另一份授權（見該目錄的 `LICENSE`）。

## 工作方式

- 有 `.codegraph/` 索引：定位符號、呼叫端與影響範圍時先用 `codegraph_explore` / `codegraph explore "<問題>"`，不要先 grep 再整份讀檔。大幅改動後執行 `codegraph index` 更新索引。
- 先想清楚再寫：先說明假設，有多種解讀時列出來讓使用者選，不要自行挑一個。
- 只改必要的部分：符合鄰近程式碼的風格，不順手重構或重排無關的程式碼；只移除自己這次改動造成的未使用 import 或變數。
- 用驗證定義完成：修 bug 先寫能重現問題的測試；完成前跑上面的 fmt、clippy 與相關測試，不為了讓測試通過而削弱既有測試。GUI 行為無法用單元測試涵蓋時，說明需要手動 `cargo run` 確認的項目。
- 功能或修正一律在 `.claude/worktrees/<name>` 開 worktree 進行（`git worktree add .claude/worktrees/<name> -b <branch> main`）。驗證通過後先離開 worktree，再從主 checkout merge 回 `main`，最後移除 worktree。純文件改動與單行 typo 可直接在 `main` 修改。
- 非單純問答的任務，依 `.claude/agents/` 的角色分派：`explorer`（定位）→ `executor`（實作）→ `verifier`（獨立驗證）→ `tech-writer`（文件／commit）。改到網路、憑證或 proxy 時，加派 `security-review`。
