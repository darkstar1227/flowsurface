# Flowsurface Fork PRD（macOS 自用版）

> 2026-11-10 依原始碼掃描修正。原版 PRD 假設多項功能需新做，實際上 upstream 已有不少基礎，以下各項皆標註「現況」與「要做的部分」。

## 1. 概述與範圍

| 項目 | 內容 |
|---|---|
| 目標 | 自用的加密貨幣看盤與交易桌面軟體，只支援 macOS |
| 基底 | fork 自 `flowsurface-rs/flowsurface`（`upstream` remote），個人 fork 為 `origin` |
| 資料來源 | 交易所 WebSocket 與 REST（Binance、Bybit、Hyperliquid、OKX、MEXC）；歷史成交可用 Off／Exchange（只限 Binance）／Server 模式 |
| 交易所 | 交易功能（F6、F7）只做 **Binance** 與 **OKX** |
| 授權 | 主程式與 `data` 是 GPL-3.0-or-later，`indicator-engine` 是 Apache-2.0，`exchange` 是 MIT（已核對各 crate 的 `Cargo.toml`）。自用不散布，無實際負擔 |
| 不在範圍 | 多週期指標（`request.security`）、策略回測、價格警報、提現、Windows 與 Linux 支援 |

## 2. 功能需求

| 編號 | 模組 | 現況（upstream 已有） | 要做的部分 | 優先級 |
|---|---|---|---|---|
| F8 | 幣種切換 | 連動群組 `LinkGroup`、`Dashboard::switch_tickers_in_group`、自選清單與搜尋排序（`data/src/tickers_table.rs`） | 快捷鍵開啟幣種搜尋；各窗格切換的操作入口更直接 | P0 |
| F9 | 分頁 | 具名 layout 可新增、改名、拖曳排序、複製、刪除，重啟後還原；只能從 `LayoutManager` 彈窗切換 | 在 dashboard 上方加分頁列，直接沿用 layout 的現有操作；快捷鍵 Cmd+T／Cmd+W／Cmd+1～9 | P0 |
| F1 | 畫線依幣種儲存 | 已有水平線、趨勢線、垂直線、矩形、測量，支援 undo/redo；但畫線存在每個窗格的 `kline::Config.drawings`，**換幣種時會被帶到新幣種** | 主價格面板的畫線改為全域依幣種共用：所有窗格、分頁中的同一幣種（交易所＋市場）看到同一組畫線；指標面板上的畫線仍屬於各自窗格 | P0 |
| F4 | `.ind` 編輯器 | 無編輯器；指標目錄沒有 watch／reload，手動安裝後需重啟 | 從左側欄開啟編輯器視窗（程式碼需要比指標側欄更寬的空間），存檔時呼叫引擎 check 並顯示行號錯誤，寫入 `indicators/` 並熱重載；內建指標唯讀，改 id 另存 | P0 |
| F10 | 版面模板 | chart preset 只存單一圖表的 `SettingsSnapshot` | 命名模板（窗格配置、週期、指標；畫線全域依幣種共用，不屬於模板）；載入時可選套用原幣種或目前幣種；匯出匯入 | P1 |
| F11 | 多視窗 | 窗格 popout 與 `WindowSpec` 已持久化 | 整個分頁拖出成新視窗，記住所在螢幕 | P1 |
| F5 | Pine 轉譯器 | 無 | 獨立 Rust crate，Pine 文字轉 `.ind`，回傳不支援語法清單，CLI 與 UI 共用 | P1 |
| F2 | 其他畫圖 | 趨勢線、矩形、垂直線已有 | 只缺 Fib 回撤線 | P1 |
| F3 | Heatmap 強化 | Heatmap（歷史 DOM）已有，含 spread／depth 指標 | 大單氣泡、掛單量色階、成交量剖面側欄；評估自行錄製 orderbook 以取得歷史熱圖；heatmap 可畫線：與同幣種 K 線共用畫線（時間＋價格定位），作為獨立疊加層、不改變 heatmap 原本顯示，每個 pane 可選擇不顯示 | P1 |
| F6 | 交易所憑證 | `keyring` 3.6.3 + `apple-native` 已用於 proxy 與 server 憑證（`data/src/config/auth.rs`） | 沿用相同模式存 Binance／OKX 金鑰；設定檔只存別名 | P0（交易前置） |
| F7 | 交易 | `exchange` 沒有任何簽名或下單程式 | 見第 3 節 | P2 |

## 3. 交易（F7）規格：Binance 與 OKX

### 3.1 訂單類型

第一版支援限價單、市價單、撤單，以及下列保護單。保護單一律**掛在交易所端**，app 關閉或斷線後仍然有效；不在本機模擬觸發。

| 功能 | Binance USDⓈ-M 合約 | Binance 現貨 | OKX（合約與現貨） |
|---|---|---|---|
| 止損 | `STOP` / `STOP_MARKET`，走 Algo Order API（`POST /fapi/v1/algoOrder`，`algoType=CONDITIONAL`、`triggerPrice`） | `STOP_LOSS` / `STOP_LOSS_LIMIT` | `order-algo` 的 `ordType=conditional`，`slTriggerPx` / `slOrdPx`（`-1` 為市價） |
| 止盈 | `TAKE_PROFIT` / `TAKE_PROFIT_MARKET`，同樣走 Algo Order API | `TAKE_PROFIT` / `TAKE_PROFIT_LIMIT` | `ordType=conditional`，`tpTriggerPx` / `tpOrdPx` |
| 止損＋止盈同時掛 | 分別下兩張 algo 單，其中一張觸發後撤掉另一張（由 app 處理，見 3.4） | `orderList/oco`；進場同時掛用 `orderList/otoco` | `ordType=oco`；或下單時用 `attachAlgoOrds` 一起帶上 |
| 追蹤止損（回撤） | `TRAILING_STOP_MARKET`（Algo Order API），參數 `callbackRate`、`activatePrice` | 止損／止盈單加上 `trailingDelta`（單位 BIPS，受 `TRAILING_DELTA` filter 限制） | `ordType=move_order_stop`，`callbackRatio` 或 `callbackSpread`，可選 `activePx` |

注意事項：

- Binance USDⓈ-M 自 2025-12-09 起，條件單改走 Algo Service：舊的 `POST /fapi/v1/order` 下這幾種類型會回 `-4120`；回傳的 ID 是 `algoId`，撤單用 `DELETE /fapi/v1/algoOrder`；**未觸發的條件單不能修改**，只能撤掉重下；觸發失敗的原因改從 user data stream 的 `ALGO_UPDATE` 事件取得。
- 以上欄位名稱目前是根據第三方文件整理，**實作前必須逐一對照官方文件核對**：Binance Derivatives「New Algo Order」與 Change Log、Binance Spot API 與 Trailing Stop FAQ、OKX API v5 `order-algo`。

### 3.2 交易所設定頁（每個交易所、每個帳戶各一份）

| 設定 | 說明 |
|---|---|
| 金鑰別名 | 對應 Keychain 項目；OKX 需 key、secret、passphrase 三項 |
| 環境 | 正式／測試網（Binance Testnet、OKX 模擬盤 `x-simulated-trading: 1`） |
| 預設止損 | 以進場價的百分比或固定價差表示，可關閉 |
| 預設止盈 | 同上，可設多段（例如 50% 在 TP1、50% 在 TP2） |
| 預設追蹤止損 | 回撤比例（Binance `callbackRate`、OKX `callbackRatio`）與啟動價條件 |
| 觸發價類型 | 最新價／標記價／指數價（Binance `workingType`、OKX `tpTriggerPxType`／`slTriggerPxType`） |
| 只減倉 | 保護單預設 `reduceOnly`，避免反向開倉 |
| 進場時自動附掛 | 下進場單時自動掛上預設的止損、止盈與追蹤止損 |
| 持倉模式 | 單向或雙向（Binance `positionSide`、OKX `posSide`），必須與交易所帳戶設定一致，連線時檢查 |
| 保證金模式與槓桿 | 逐倉或全倉、預設槓桿；下單前確認與交易所一致 |

### 3.3 帳戶風控（本機）

「回撤」也包含帳戶層級的最大回撤保護。這部分在本機計算，**只在 app 執行中有效**，UI 必須標明這點。

- 單筆上限（數量或名目價值）、單一幣種最大持倉、單日最大虧損。
- 帳戶權益從當日（或自訂起點）高點回撤超過設定比例時：停止所有新進場單；可選擇自動撤銷所有掛單並市價平倉。
- 一鍵撤銷全部委託；一鍵全部平倉。
- 觸發風控時寫入本機日誌並跳出通知。

### 3.4 其他交易需求

- 簽名下單、撤單、查持倉與餘額、私有 WebSocket（訂單、成交、持倉、algo 單更新）。
- DOM 點價下單；圖表上可拖曳止損、止盈線，拖曳等於撤掉舊單再下新單（Binance 條件單不可修改）。
- 委託、algo 單、持倉列表，標示各持倉目前掛著哪些保護單。
- 止損＋止盈若是兩張獨立單（Binance 合約），其中一張成交時，app 收到 user data stream 通知後撤掉另一張；app 未執行時則依賴 `reduceOnly` 防止反向開倉。
- 睡眠喚醒後私有 WebSocket 自動重連，並以 REST 重新同步委託與持倉。

### 3.5 憑證與安全

- 金鑰只存 Keychain（`keyring` 的 `apple-native`，已在 `data/Cargo.toml` 啟用），記憶體中用 `secrecy` 包裝，不寫入 log。
- 連線測試時檢查金鑰權限：有提現權限就拒絕使用。
- 用子帳戶金鑰並綁 IP。Binance HMAC 金鑰沒綁 IP 時只能有讀取權限；OKX 子帳戶的交易金鑰必須綁 IP。
- OKX 下單端點有限流；Binance 依 weight 與下單數限流。沿用 `exchange/src/adapter/limiter.rs`。
- 指標與交易完全隔離，指標不能觸發下單。
- 完整安全條件與後續項目見 docs/trading-security.md。

## 4. 其他關鍵規格

**Pine 轉譯（F5）：**
- 可轉：`ta.sma/ema/rsi/pivothigh`、`input.*`、`var`、`plot`、`bar_index`。
- 部分對應：`line/box/label` 轉成 `segment`／`region`。
- 不可轉並須明確報錯：`request.security`、`strategy.*`、`alert`、`varip`、map、table。
- `.ind` 檔頭需要 `language(1)` 與 `indicator("標題", id = "namespace.name")`，`plot` 必須有 `id`。指標沙盒有算力上限，不能存取網路、檔案或下單（見 `indicator-engine/LANGUAGE.md`）。
- 轉譯後用 CLI 的 `check` 與 `test` 驗證；UI 標示哪幾行沒轉成功。

**狀態模型：**
- 模板存「形狀」，分頁（layout）存「內容」，模板可套到不同幣種。
- 主面板畫線存在全域的 `layout::drawings::Library`（依幣種），跟著 `saved-state.json` 的 layouts 一起存；切換幣種不會串線。
- `saved-state.json` 目前沒有版本欄位，靠 serde 的 default 與自訂 deserializer 維持相容；第一次需要不相容的格式變更時再加 `schema_version`。
- 背景分頁暫停重繪；Heatmap 要錄資料的窗格保持訂閱。

**macOS：**
- 只需 Xcode Command Line Tools，`cargo run --release` 即可。
- 自行編譯版本沒有 Apple 簽名，Keychain 在重新建置後可能重複詢問授權，選「永遠允許」。
- 原生快捷鍵（Cmd+T、Cmd+N、Cmd+W），記住多螢幕位置。
- 睡眠喚醒後 WebSocket 自動重連並補資料。

## 5. 里程碑

| 階段 | 內容 | 驗收標準 | 狀態 |
|---|---|---|---|
| M0 | F1 畫線依幣種儲存、F9 分頁列與快捷鍵、F8 快捷鍵 | 換幣種不串線、換回來畫線還在；重啟後還原分頁、幣種與畫線 | 已合併 |
| M1 | F4 編輯器 | 編輯器顯示編譯錯誤並熱重載 | 已合併：側欄「Indicator editor」視窗；存檔時檢查並列出行:欄錯誤，寫入 `indicators/`，熱重載並重建使用該指標的圖表；目錄安裝與移除也不需重啟 |
| M2 | F10 模板、F11 多視窗、F5 轉譯 CLI | 模板可套用到另一幣種；3 到 5 個 Pine 腳本轉譯後通過 `check` | 已合併（F11 未做分頁重新排序；F5 的 line/box/label 對應未做） |
| M3 | F2 Fib、F3 Heatmap 強化 | 所有畫圖工具可選取、移動；氣泡與色階可調 | 已合併（heatmap 畫線不含測量、磁吸、框選；歷史熱圖錄製只做評估） |
| M4 | F6 Binance／OKX 憑證與設定頁 | 新增、測試、刪除金鑰；有提現權限時被拒絕；持倉模式不一致時提示 | 已合併（簽名請求共用限流、下單前讀取判定留到 M5；Binance Demo Trading 金鑰目前不支援） |
| M5 | F7 交易 | 測試網／模擬盤完整走過：下單、撤單、止損、止盈、追蹤止損觸發、成交回報、帳戶回撤風控觸發；之後才用小額正式單 |  |

## 6. 風險與待決

- **歷史熱圖：** 交易所沒有公開歷史 orderbook 快照，需自行長期錄製。
- **Pine 覆蓋率有限：** UI 必須標示哪幾行沒轉成功。
- **合併衝突：** 一個功能一個分支，UI 改動盡量放在新增檔案。
- **API 變動：** Binance 已在 2025-12 改過條件單 API，實作 F7 前要重新查兩家交易所的 changelog。
- **本機風控的限制：** 帳戶回撤保護只在 app 執行中有效；真正的保護靠交易所端的止損單。
- **已決定（2026-11-10）：**
  - 交易只做手動下單；指標與訊號不能觸發下單。
  - OKX 只支援 Global（`www.okx.com`／`openapi.okx.com`），UI 註明不支援 US／EEA 帳戶。
  - Binance 現貨納入 M5。
  - LIVE 帳戶一開始就能送單，每次送出前顯示 LIVE 警示並要求二次確認。
- **官方文件核對後的差異（M5 設計依據）：**
  - 雙向持倉下 Binance 與 OKX 都不能送 `reduceOnly`，保護單改靠 `positionSide`／`posSide` 與 `closePosition`／`closeFraction`。
  - OKX 單張 `conditional` 同時帶 TP 與 SL 只會執行 SL，兩者並存必須用 `oco` 或 `attachAlgoOrds`。
  - 下單取整要用交易所原始的 tickSize／stepSize 字串，不能用 `MinTicksize`（它會四捨五入成 10 的次方）。
