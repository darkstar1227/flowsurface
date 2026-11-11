# 交易安全條件

本文件列出交易所憑證與交易功能必須遵守的安全條件。M4（交易所憑證與設定頁）已依這些條件實作；M5（交易）的每個改動都要對照本文件。條件編號沿用 M4 安全審查時的編號，變更條件時須同步更新本文件。

## 條件 1–37

### A. Keychain 儲存

1. service 名稱用 `flowsurface.exchange`，不得與既有的 `flowsurface.proxy`、`flowsurface.server` 共用。
2. account 名稱用帳戶的 UUID，格式為 `<venue>:<uuid>`。不得用別名，因為別名可以改名，也可能重複。
3. 每個帳戶只存一個 Keychain 項目，內容為 JSON，包含 venue、env、key、secret；OKX 另含 passphrase。刪除只需呼叫一次。若拆成多個項目，必須逐一刪除，並把任何失敗回報到 UI。
4. 環境與交易所寫進 Keychain 項目。載入時與 `saved-state.json` 的 `env`、`venue` 比對，不一致就把帳戶標記為不可用，並在 UI 說明原因。
5. save、load、delete 都回傳 `Result`，錯誤交給 UI 顯示；不得只寫 log，或回傳 `()` / `None`。
   - 存檔：先寫入 Keychain 並讀回比對成功，才把帳戶中繼資料寫進 `saved-state.json`。寫入失敗時，不得留下沒有對應 Keychain 項目的帳戶。
   - 刪除：`NoEntry` 視為成功。其他錯誤要保留該列並標示「刪除失敗」，不得假裝已刪除。
   - 載入：區分 `NoEntry`（顯示「憑證遺失」）與其他錯誤（例如 Keychain 鎖住或使用者拒絕存取），不得一律回傳 `None`。
6. keyring 錯誤只能用 `{}`（Display）輸出，禁止用 `{:?}`。`keyring::Error::BadEncoding(Vec<u8>)` 的 Debug 會印出原始位元組，也就是 secret 本身。
7. Keychain JSON 解析失敗時，只記錄「invalid format」，不得把 `serde_json::Error` 的文字寫進 log 或 UI，因為 serde 錯誤訊息會引用部分輸入值。
8. log 中只能出現 service、venue、UUID、別名；不得出現 API key（即使只露出部分）、secret、passphrase。

### B. 記憶體與 Debug

9. secret 與 passphrase 在記憶體中一律用 `secrecy::SecretString`（0.10.x）。API key 也要用 `SecretString` 或自訂型別包起來，Debug 時遮罩。只有在簽名函式內部，以及送出 header 前的那一刻，才能呼叫 `expose_secret()`。
10. 序列化成 JSON 的中介字串（寫入 Keychain 前、從 Keychain 讀出後）用 `zeroize::Zeroizing<String>` 包起來。
11. 任何帶有 key、secret、passphrase 的型別不得使用 `#[derive(Debug)]`，須手寫 Debug 並輸出 `***`。modal 的 `Message` 中帶明文輸入的 variant 也一樣，需包一層手寫 Debug 的 newtype。
    - 理由：`debug` feature 會啟用 iced 的 `hot`，`iced_debug` 會對每個 Message 執行 `format!("{message:?}")`，並把前 49 個字元送到 comet beacon。例如 `ExchangeAccounts(SecretChanged("` 約 30 個字元，剩下約 19 個字元的 secret 就會外洩。
12. 必須有單元測試：對帳戶型別、憑證型別、表單型別、Message variant 做 `format!("{:?}")`，斷言結果不含測試用的 secret、key、passphrase。
13. 新 modal 開啟時不得把已存的 secret 回填到輸入框。已存的帳戶只顯示別名、交易所、環境，以及 API key 後 4 碼以外全部遮罩（或完全不顯示）。修改憑證等於刪除後重新新增。
14. secret 與 passphrase 欄位用 `secure(true)`，不提供「顯示明文」切換。
15. 存檔（不論成功或失敗）與關閉 modal 時，清空所有輸入欄位；可行時先用 `zeroize()` 覆寫再丟棄。Test 的結果只保存判定結果，不保存回應內容。

### C. 簽名請求的傳輸

16. Base URL 一律寫死為 `https://` 常數，依 (venue, env) 對應；不得來自 `saved-state.json`、使用者輸入或 `server_url`。簽名前檢查 scheme 為 `https`，並確認 host 在白名單內。不符合時回傳錯誤，不得送出。
17. 簽名請求的 reqwest client 需沿用 timeout 與 proxy 設定，並加上 `.redirect(reqwest::redirect::Policy::none())` 與 `.https_only(true)`。
    - 理由：reqwest 0.12 跨 host 轉址時只會移除 `Authorization`、`Cookie`、`Proxy-Authorization` 等標準標頭，不會移除 `X-MBX-APIKEY`、`OK-ACCESS-KEY`、`OK-ACCESS-SIGN`、`OK-ACCESS-PASSPHRASE`。既有的 `exchange_http` 使用預設轉址策略（最多 10 次）。
    - M4 的實作方式：以 `exchange/src/signed_http.rs` 的 `SignedHttp::build` 建立（單一建構函式，欄位私有，`TradeClient::new` 與 `StreamContext` 只收 `SignedHttp`），未經 `http_client(...)`。原因是 `http_client` 建立失敗會 panic，套用 proxy 失敗時會改為直連，兩者都與條件 21 衝突。此替代做法已審查通過，且達成條件 17 與 21 的目的。
18. 簽名請求絕不可經過 `server_http`，因為它對 `danger_accept_invalid_certs(true)`。簽名用 client 不得呼叫 `danger_accept_invalid_certs`、`danger_accept_invalid_hostnames`，也不得加入自訂 root CA。新檔案中不得出現 `danger_`。
19. 不得直接使用 `HttpHub::http_*_with_limiter` 送簽名請求，原因有二：`send_request_client` 無法加 header；URL 會原封不動寫進 log 與錯誤字串，包括 query 中的 `signature`。
    - 簽名請求改用專屬函式，可與既有程式共用 limiter 實例，但需符合：
      - log 與錯誤只帶 `method + host + path`，去掉整個 query。
      - 不得使用會帶 URL 的錯誤建構函式（例如 `request_failed`、`response_body_failed`），也不得把 `reqwest::Error` 用 `{}` 或 `{:?}` 輸出（兩者都會包含 URL）。只取 `is_timeout()`、`is_connect()`、`status()` 等分類。
      - 私有端點的回應解析失敗，或 HTTP 狀態非 2xx 時，只記錄交易所的錯誤碼（`code`）。`msg` 只顯示在 UI，不寫進 log。不得記錄完整回應內容預覽。
20. 必須有測試：故意產生錯誤（或直接對錯誤建構函式做單元測試），斷言錯誤字串與 log 字串不含 `signature=`、`OK-ACCESS-SIGN`、測試用的 key 與 secret。
21. proxy fail closed：使用者有設定 proxy，但套用失敗時，簽名用 client 必須拒絕建立，並在 UI 顯示錯誤，不得改為直連。可另寫一個回傳 `Result` 的變體，不改動既有行為。
22. proxy 說明（報告中需寫明）：
    - reqwest 透過 HTTP 或 HTTPS proxy 連 `https://` 目標時使用 CONNECT，proxy 只看得到目標 host 與 port，看不到 header 與 query。
    - SOCKS5 與 SOCKS5h 同樣看不到內容。
    - reqwest 使用 `rustls-tls`（webpki roots），不信任系統加裝的 MITM CA，因此企業 MITM proxy 只會讓連線失敗，不會讓內容外洩。
    - 只要條件 16 強制 `https`，就沒有明文經過 proxy 的風險。

### D. 權限判定（fail closed）

23. 判定為純函式，輸出三種狀態：`Usable`、`Rejected(reason)`、`Unverified(reason)`。解析不到、欄位缺少、型別不符一律視為不安全。權限欄位不得加 `#[serde(default)]`，否則缺欄位會變成 `false`，等於「無提現權限」。反序列化改用 `Option<bool>`，或保留 `serde_json::Value` 再取值。
24. Binance（欄位名稱已依官方來源核對，見下文）：
    - `GET /sapi/v1/account/apiRestrictions` 回應中 `enableWithdrawals == true` → `Rejected`。欄位缺少、HTTP 錯誤或無法解析 → Live 環境判為 `Rejected("無法確認提現權限")`，不得視為通過。
    - 能移動資金的其他旗標（例如 `enableInternalTransfer`、`permitsUniversalTransfer`）須在報告中列出，至少顯示警告；若可把資金轉到他人帳戶，視同提現權限，直接拒絕。
    - `ipRestrict == false` → 警告。
25. OKX：
    - `GET /api/v5/account/config` 的 `perm` 是逗號分隔字串。以 `perm.split(',').map(str::trim)` 完全比對是否有 `withdraw` token，不得用 `contains("withdraw")`。
    - `perm` 缺少、`code != "0"`，或 `data` 為空 → `Rejected` 或 `Unverified`，不得視為通過。
    - `ip` 為空字串 → 警告。
    - `posMode` 只接受 `net_mode` 與 `long_short_mode`，其他值顯示「未知持倉模式」警告。
26. 測試網或模擬盤無法查詢權限時（例如 Binance futures testnet 沒有 `/sapi`）：狀態為 `Unverified`，UI 明確顯示「無法確認權限（測試網）」。只有 `env == Testnet` 的帳戶可在 `Unverified` 下使用；`env == Live` 的帳戶必須是 `Usable`。
27. 判定結果只保存在記憶體，每次啟動 app 或每次按 Test 都重新計算。不得把「已通過」「無提現權限」之類的布林值寫進 `saved-state.json` 再讀回信任，因為檔案可以被人改過。M5 下單前，要求當次 session 已取得 `Usable`（測試網可為 `Unverified`）。
28. 持倉模式不一致時只顯示警告，不阻擋下單。比對時使用交易所回傳的實際值，不使用存檔值。

### E. 測試網與正式環境混用

29. 帳戶建立後 `env` 與 `venue` 不可修改；要改就刪除後重新新增。UI 不提供修改環境的控制項。
30. Base URL 與 OKX 的 `x-simulated-trading` header 由 Keychain 項目內的 `env` 決定。若與 `saved-state.json` 的 `env` 不一致，拒絕送出任何請求。即使有人改過 JSON，也無法把正式環境的金鑰導向別的 host，或讓模擬盤實際打到正式環境。
31. OKX：Live 環境一定不送 `x-simulated-trading`；Testnet 環境一定要送 `x-simulated-trading: 1`。以單元測試鎖定這兩種 header 組合。
32. UI 在帳戶清單與 Test 結果中明顯標示 `LIVE` 或 `TESTNET`。

### F. 其他

33. 時間戳：
    - Binance 的 `recvWindow` 不得超過 5000，並先呼叫公開的 server time 端點取得時間差再校正。`-1021` 錯誤要轉成「本機時鐘誤差過大」的訊息。
    - OKX 的時間戳使用 UTC ISO 8601 毫秒格式（例如 `2020-12-08T09:08:57.715Z`），簽名內容的 requestPath 要包含 query string。
    - 不得自動重試同一個已簽名的請求；重試時要重新產生時間戳與簽名。
34. Limiter：
    - Test 按鈕在請求進行中要停用，防止連點。
    - 遇到 429 或 418（Binance）就停止並顯示訊息，不得進入重試迴圈。
    - Binance 的 `/fapi` 簽名請求要與公開的 `/fapi` 請求共用同一個 limiter 實例，因為同一 IP 的權重是一起計算的。做不到時須在報告中說明。
35. 依賴：
    - HMAC 二選一：(a) 直接使用 lockfile 已有的 `ring` 0.17.14 的 `ring::hmac::HMAC_SHA256`，不新增 crate（建議）；(b) 用 RustCrypto 時，`hmac` 與 `sha2` 須搭配同一個 `digest` 主版本，不得混用 `hmac` 0.12 與 `sha2` 0.11 這類組合。
    - `base64` 0.22.1 沿用既有版本。`secrecy` 0.10.3、`zeroize` 1.8.x 從 crates.io 取得。
    - 不得使用 git 或 path 來源；`Cargo.lock` 的 checksum 必須存在。
    - 簽名函式要用官方文件的範例向量測試。
36. `saved-state.json` 不得含任何機密：
    - 存檔用的帳戶型別本身不得有 key、secret、passphrase 欄位，不得用「有欄位但加 `#[serde(skip)]`」的方式處理。
    - 測試：建立含假 secret 的帳戶，序列化整個 `State`，斷言 JSON 不含假 secret、假 key、假 passphrase 字串。
    - layout 模板的匯出與匯入（F10）也不得包含帳戶資料。
37. Linux：`linux-native`（kernel keyutils）的金鑰在重新開機後會消失，畫面顯示「憑證遺失」。不會造成外洩，M4 不處理，但須在報告註明。

## M4 實作位置

- `exchange/src/account.rs`：簽名請求、`Verdict` 判定、`is_usable`、`check_account`；host 白名單 `allowed_hosts` 與 `endpoint_url`。
- `data/src/config/exchange_accounts.rs`：Keychain 存取（`KEYCHAIN_SERVICE = "flowsurface.exchange"`）、帳戶中繼資料、`sanitized()`。
- `src/modal/exchange_accounts.rs`：設定頁 modal、表單、Test、Save 與 Delete。
- `exchange/src/adapter/proxy.rs` 的 `apply_proxy_strict`：簽名用 client 的 proxy fail closed。
- `exchange/src/signed_http.rs` 的 `SignedHttp`：簽名用 client（`redirect(none)`、`https_only(true)`、嚴格 proxy，proxy 壞掉時不建立）。GUI 在 `src/connector/client.rs` 只負責呼叫 `SignedHttp::build`。

## M5 交易

M5 的安全審查（唯讀）找出的風險，依嚴重度排列：

- **R1（高）**：`send_get` 在 2xx 但無法解析、超過 1 MiB、或中途斷線時回 `BadResponse`／`Network`。對下單來說結果是**未知**，不是失敗；若沿用兩態，使用者重按就會重複下單（條件 39、60）。
- **R2（中）**：`connect_ws` 接受 `ws://`，且沒有 host 白名單；私有流組錯 URL 時，OKX login frame（含 apiKey、passphrase、sign）會明文送出（條件 40、46）。
- **R3（中）**：`allowed_hosts` 只依 (venue, env) 區分，無法表達 Binance 現貨測試網與合約測試網的差異（條件 40、41）。
- **R4（中）**：F10 只檢查簽名 REST；私有 WS 的 proxy 在 `DataSources::new` 時固定。另外，proxy 載入失敗時 `network.proxy` 是否變成 `None`（直連）**未核對**（條件 42）。
- **R5（中）**：`exchange_error` 只看頂層 `code`。OKX 下單回 HTTP 200、`code="0"`，但 `data[].sCode` 可能不為 0（條件 77）。
- **R6（低）**：`binance_clock_offset` 失敗時回 0；下單只會得到 `-1021`，但不得自動重試（條件 76）。
- **R7（低到中）**：`exchange/src/error.rs` 的 `String::truncate(177)` 遇多位元組字元會 panic，私有 WS 的 adapter task 因此中斷（條件 52、77）。
- **R8（資訊）**：`adapter/http.rs:94` 限流時會 `log::warn!` 完整 URL；簽名請求不得走這條路徑（條件 19）。
- **摘要第 8 點（中）**：`data` 依賴 `exchange`，crate 邊界不足以隔離指標與交易（條件 75）。

既有且已確認安全、可沿用：簽名 client 不跟隨 redirect、強制 `https_only`、proxy fail closed（`src/connector/client.rs`）；WS 的 TLS 只信任 webpki roots，沒有 `danger_`；`Proxy` 的 Display 會遮罩帳號密碼；`send_get` 只記 `GET host path` 且回應上限 1 MiB。

段的代號：**a** = trade-core、**b** = trade-stream、**c** = trade-ui、**d** = trade-defaults、**e** = trade-risk、**f** = trade-chart。

### 條件 38–81

以下保留每條的要求與驗證方式，措辭已精簡。「未核對」的標註保留，實作前要先核對。

**G. 簽名下單的傳輸（POST／DELETE）**

- **38（a）**：把 `send_get` 泛化為 `send(method, url, headers, body)`，保留既有安全屬性：log 與錯誤只帶 `METHOD host path`；body 與 query 一律不寫入 log，也不放進錯誤字串；回應 1 MiB 上限；不跟隨 redirect；`https_only`；`reqwest::Error` 只取分類。下單成功時也不記錄回應內容（含 orderId、數量、價格）。
  - 驗證：單元測試裝上測試 logger，用假 secret、假 key 走成功、4xx、5xx、逾時四條路徑，斷言 log 與 `Display` 都不含 `signature=`、`OK-ACCESS-SIGN`、假 key、假 secret，也不含 body 字串。
- **39（a）**：下單、撤單、algo 單的結果分三態：`Accepted{ids}`、`Rejected{code,msg}`、`Unknown{client_id}`。
  - `Unknown` 包括：送出後逾時或連線中斷、HTTP 5xx、2xx 但 body 無法解析、超過上限或缺 id、OKX `code="0"` 但 `data` 為空。
  - 依錯誤碼判為 `Unknown`（即使 HTTP 4xx 或帶 `code`）：Binance `-1006`（UNEXPECTED_RESP）、`-1007`（TIMEOUT）（`BS/errors.md:35-41`、`BS/rest-api.md:115-120`；合約是否同碼**未核對**）；OKX `50004`（`OKX:63891`）、`51149`（`OKX:64762`）。
  - 交易所明確拒絕（Binance 4xx 帶 `code`、OKX `sCode≠0`）才是 `Rejected`。`Unknown` 不得在 UI 顯示為「失敗」。
  - 依據：R1；Binance 5XX 的執行狀態是 UNKNOWN（`BS/rest-api.md:141-142`）。
  - 驗證：表格式單元測試，(status, body, 傳輸錯誤) → 三態，每列一個 case；`-1006`、`-1007`、`50004`、`51149` 都對應到 `Unknown`。
- **40（a）**：白名單依 **(venue, env, product, scheme)** 寫死，分 `https_endpoint` 與 `wss_endpoint` 兩個函式。`wss_endpoint` 只接受 `wss`、443 埠，拒絕 `ws://` 與 8443。env 取自 Keychain 項目（沿用條件 30）。
  - Binance Live：REST `api.binance.com`（現貨、`/sapi`）、`fapi.binance.com`；WS `fstream.binance.com`（合約私有流路徑 `private/stream` 依 SDK 推得，**部分核對**）、`ws-api.binance.com`（`BS/web-socket-api.md:105`）。
  - Binance Testnet 合約：REST `testnet.binancefuture.com`；WS `fstream.binancefuture.com`（私有流路徑**未核對**，(b) 開工前用測試網確認）。
  - Binance Testnet 現貨：REST `testnet.binance.vision`；WS `ws-api.testnet.binance.vision`。
  - OKX Live：REST `www.okx.com`、`openapi.okx.com`；WS `ws.okx.com`。
  - OKX Testnet：REST `www.okx.com`、`openapi.okx.com`（加 `x-simulated-trading: 1`）；WS `wspap.okx.com`。
  - 不收：`us.okx.com`、`eea.okx.com`、`demo-*`、`stream.binance.com`（現貨舊 listenKey）。
  - 依據：R2、R3。Live 金鑰連上 `wspap.okx.com`、或 Testnet 金鑰連上 `ws.okx.com`，環境就混用了；`ws://` 會讓 OKX passphrase 明文傳送。
  - 驗證：表格式測試，列舉 (venue, env, product, scheme, host, port) 所有組合，只有上表通過；特別包含 Live 不能連 `wspap.okx.com`、Testnet 不能連 `ws.okx.com`／`fstream.binance.com`、`ws://` 與 8443 被拒。
- **41（a）**：Binance Testnet 帳戶必須綁定產品（現貨或 USDⓈ-M）。產品寫進 Keychain 項目並與存檔比對（比照條件 4、30）；合約測試網金鑰絕不送往 `testnet.binance.vision`，反之亦然。Live 金鑰兩個產品共用。
  - 驗證：Testnet 合約帳戶對現貨 host 呼叫 `https_endpoint` 會被拒；Keychain 與存檔的產品不一致時拒絕載入。
- **42（a、b）**：F10 延伸。每次簽名請求、每次私有 WS 連線與重連之前，都比對目前的 `network_config.proxy` 與建立連線時的 proxy 快照，不一致就拒絕並提示重啟。私有 WS 的 proxy 與簽名 client 取自同一份快照，不得沿用 `spawn_venues` 的 proxy。proxy 設了但無法載入時，要用明確狀態（例如 `ProxyState::Broken`），不得變成 `None`（直連）。實作前先核對 R4 的載入路徑。
  - 驗證：proxy 快照比較函式與 Broken 拒絕連線的單元測試。手動（F9 一併驗）：設定無效 proxy 後下單與開私有流，都應被拒，並用 `lsof -i` 或防火牆紀錄確認沒有直連交易所。
- **43（a、b）**：不新增任何 `danger_`、自訂 root CA、憑證 pinning 例外；私有 WS 沿用 `TLS_CONNECTOR`（webpki roots）。簽名 REST 只用 `SignedHttp`，不得用 `exchange_http`、`server_http`、`HttpHub`。
  - 驗證：`git diff main -- . | grep -n 'danger_'` 結果必須為空；code review 確認呼叫點。
- **44（a）**：送出的位元組必須等於被簽名的字串。Binance 以固定順序做 form-urlencode，簽名後原樣送出；OKX body 先用 serde 序列化成字串，簽這個字串，再送出同一字串。symbol／instId 必須存在於交易規則清單，字元限制為 `[A-Z0-9-]`，不得直接放入使用者自由輸入的文字。
  - 依據：不一致會造成 `-1022`；也可能被參數注入（例如 symbol 中塞 `&quantity=`）。
  - 驗證：golden 測試，builder 輸出的簽名字串等於送出的 body 或 query；含 `&`、`=` 的 symbol 被拒絕。

**H. 私有 WebSocket**

- **45（b）**：listenKey 視為機密：以 newtype 包裝，手寫 Debug 輸出 `***`，不實作 Display、Serialize；不得出現在 log、錯誤字串、`Message`，也不得放進 iced subscription identity 的 Hash／Debug。私有 WS 的 log **只記 host**，不記 path 與 query（Binance 合約的 listenKey 在 URL 裡）。subscription identity 使用 (account uuid, venue, env, product, generation)。
  - 依據：listenKey 外洩後，有效期內任何人都能訂閱該帳戶的訂單與持倉；`debug` feature 會把 `Message` 前 49 字元送到 comet（條件 11）。
  - 驗證：對 listenKey 型別、私有事件型別、`Message::AccountWsEvent`、subscription identity 做 `format!("{:?}")`，斷言不含測試 listenKey。code review：私有 WS 模組的 `log::` 不得帶 url 或 path 變數。
- **46（b）**：送出的認證 frame（OKX `login` 的 apiKey、passphrase、sign；Binance WS API 的 apiKey、signature）一律不寫入 log。收到的 frame 解析失敗時，只記事件型別與長度，不記原文；登入失敗只記 code。`WsTransport::write_frame` 與讀取路徑是否記錄 frame 內容**未核對**，(b) 開工時先確認。
  - 驗證：用測試 logger 走一次假 login 失敗路徑，斷言 log 不含假 key、passphrase、sign。code review：`ws.rs` 內所有 `log::` 呼叫。
- **47（b）**：OKX WS login 用新的 `okx_ws_timestamp`（Unix 秒），prehash 為 `ts + "GET" + "/users/self/verify"`。每次連線與重連都重新呼叫 `load_credentials` 並重新簽名；frame 送出後立即釋放憑證，adapter 結構體不保存 secret 與 passphrase。登入被拒（認證類錯誤）時停止、標示「認證失敗」、**不自動重連**；只有網路錯誤才 backoff。
  - 依據：`OKX:703,782-787`；同一連線 login／subscribe 合計 480 次／小時、單 IP 每秒 3 條連線。認證失敗若進入重連迴圈，會被交易所封鎖。
  - 驗證：以官方範例的 timestamp 與 prehash 格式、假 secret 做簽名向量測試；狀態機測試：認證失敗 → `AuthFailed`，不排程重連。
- **48（b）**：Binance 現貨私有流走 WS API `userDataStream.subscribe.signature`（HMAC，`recvWindow ≤ 5000`）。不用 `session.logon`（只支援 Ed25519），也不用已退場的舊 listenKey。文件總則寫明支援 HMAC（`BS/web-socket-api.md:114`），傾向可行，**但需測試網實測**。實測前與訂閱失敗時一律 fail closed：現貨私有流標為「不可用」，並依使用者決定 3 停用現貨交易。
  - 依據：`BS/web-socket-api.md:8052-8070,8174-8205`；`BS/CHANGELOG.md:740-770`。
  - 驗證：簽名參數依字母排序、簽名結果符合預期的單元測試。手動：在現貨測試網實際訂閱一次，記錄成功與否。
- **49（b）**：Binance 合約 listenKey 生命週期：透過簽名 client 建立（經 limiter），每 ≤30 分鐘 `PUT` 延長。收到 `listenKeyExpired` 或 `PUT` 失敗時，重建 listenKey、重連，並以 REST 重新同步。帳戶刪除或停用時 `DELETE` 該 listenKey 並丟棄。listenKey 與 account uuid 綁定，不跨帳戶重用。
  - 依據：listenKey 60 分鐘後失效；沒察覺的話，app 會以為沒有成交事件，「撤另一張」就不會執行。
  - 驗證：狀態機單元測試，用假時鐘模擬到期事件與 PUT 失敗。手動：測試網上等待超過延長週期。
- **50（b）**：重連沿用 `ReconnectBackoff`（500 ms 起、倍增、上限 30 秒、含抖動，`ws.rs:937-956`）。OKX 需要 `private` 與 `business` 兩條登入連線，各自 backoff；每條連線開始前先向 limiter 取得連線配額（每 IP 每秒 3 條）。
  - 驗證：backoff 序列有上限且帶抖動；同時斷線時，連線請求速率不超過配額。
- **51（b、c）**：每個帳戶的私有流狀態為 `Connected`／`Reconnecting`／`Resyncing`／`AuthFailed`／`Unavailable`，在 UI 明顯顯示。狀態不是「`Connected` 且已重新同步」時：委託與持倉列表標為「可能過期」；**禁止新進場單與拖曳改單**；撤單、平倉、撤全部、全平仍允許；「一張成交撤另一張」暫停，重連後先以 REST 同步再補做。重新同步的端點：Binance 合約 `positionRisk v3`、`openOrders`、`openAlgoOrders`；現貨 `openOrders`、`openOrderList`、`account`；OKX `positions`、`orders-pending`、`orders-algo-pending`（`orders-algo` 不推快照）。睡眠喚醒視同斷線。
  - 依據：依過期資料下單或拖曳，會撤錯單，或在已平倉時又掛出新單。
  - 驗證：狀態機測試，每個狀態下哪些動作允許、哪些拒絕。手動：測試網斷網 30 秒後恢復，確認重新同步完成前不能新進場。
- **52（b）**：私有事件是不受信任的輸入：不得 `unwrap`、不得 panic；數值以十進位字串解析，不經過 f32；未知事件型別只記型別名稱並忽略；未知訂單狀態一律觸發重新同步，不猜測語意。單一 frame 大小上限沿用既有 WS 設定（是否有上限**未核對**）。
  - 依據：R7。adapter task 崩潰會讓私有流中斷，而 UI 未必察覺。
  - 驗證：解析器測試，以官方文件範例為固定檔，另加缺欄位、型別錯誤、超長字串、未知 `X` 狀態等變形輸入，斷言回傳錯誤、不 panic。

**I. 共用限流（F1）**

- **53（a）**：Binance IP weight：簽名與公開請求共用**同一個** limiter 實例（依 base host 區分，`api.binance.com` 與 `fapi.binance.com` 各一個）。實作可用 `Arc<Mutex<_>>` 或 limiter command channel；現況下實例被搬進 worker task，無法共用（依 explorer 報告 2.2）。追蹤下單計數：合約讀 `X-MBX-ORDER-COUNT-10S`／`-1M`；現貨依 `X-MBX-ORDER-COUNT-(intervalNum)(intervalLetter)` 樣式讀取（`BS/rest-api.md:233-237`；被拒的單不保證帶此標頭）。實際區間依 exchangeInfo 的 `rateLimits` 決定，不寫死。上限數值**未核對**。
  - 依據：條件 34；同一 IP 的 418 封鎖會連行情一起中斷。
  - 驗證：假回應標頭驅動 limiter，斷言公開與簽名請求消耗同一份額度；計數接近上限時拒絕新進場單。回歸：既有行情 fetch 行為不變。
- **54（a）**：OKX 依端點建立限流桶：下單與撤單 60 次／2 秒（依 instId）；`order-algo` 與 `cancel-algos` 各 20 次／2 秒；`orders-algo-pending` 20 次／2 秒；`positions` 與 `balance` 各 10 次／2 秒。OKX 依 User ID 計算，不需與公開行情共用。
  - 依據：現有 `FixedWindowRateLimiter` 是單一全域視窗（依 explorer 報告），查詢流量會擠掉撤單額度。
  - 驗證：每個桶獨立計數，互不影響。
- **55（a）**：優先級與保留額度：撤單、平倉、保護單 ＞ 查詢 ＞ 新進場。新進場單不得用掉每個桶最後 20%（暫定值）的額度；撤單與保護單可用到 100%。
  - 驗證：額度用到 80% 後，新進場單被拒，撤單與保護單仍可送出。
- **56（a）**：429 與 418：該桶依 `Retry-After` 暫停；**下單類請求不自動重試**。Binance 418（IP 被封）時暫停**所有** Binance 請求，包括公開行情 fetch，直到 `Retry-After` 結束，並在 UI 顯示。保護單遇 429 或 418，視同送出失敗，觸發條件 65 的裸倉警示。不得進入任何重試迴圈。
  - 依據：條件 34；遇到 418 還繼續送，封鎖時間會延長。
  - 驗證：429 → 該桶暫停、狀態為 `Rejected(RateLimited)`、沒有重送；418 → 全域暫停旗標生效，公開 fetch 也被擋。

**J. 下單前判定（F2）**

- **57（a、c）**：下單前有兩層判定，兩層都通過才送出。
  1. **F2 閘門**（純函式，`data::config::trade_gate`）：`can_send(session_verdict, keychain_meta, saved_meta, proxy_ok, order)`，由 `SessionGate` 以唯一的 `SendGate` 實作提供。私有流狀態不是它的參數：data 不再保存第二份串流狀態（`StreamState`、`SessionGate.stream` 已移除），避免 UI 與送單讀到不同的值。
  2. **exchange 的 `approve`**：從以帳戶為 key 的串流狀態 registry 讀取私有流狀態（由私有流的 driver 寫入，送單端與 GUI 都不能寫）。非 `Connected` 拒絕新進場與拖曳，沒有登記等於 `Reconnecting`，現貨 `Unavailable` 等於現貨交易停用；本機風控 `RiskHook`（唯一來源是 `EquityTracker` → `RiskSnapshot`，設定無效等於鎖定新進場）；規則有效期；未解決 Unknown 的 (帳戶, symbol) 鎖。
     - c2a 補強：registry 為 `AuthFailed` 時 `approve` 拒絕**所有**單，含平倉與減倉（條件 57：登入失敗代表 session 作廢、憑證不可用，舊的 `can_send` 也是全拒）。每次重新同步的 Snapshot 帶一個 `sync_epoch`；`EquityTracker` 必須對該 epoch 呼叫 `snapshot_applied`，否則新進場以 `SnapshotNotApplied` 被拒（串流回到 Connected 但快照尚未套用的空窗）。driver 的 drop guard 只把 Connected／Resyncing 降為 Reconnecting，不覆寫 `AuthFailed`／`Unavailable`。

  本機風控不是 `can_send` 的參數，UI 的鎖定顯示也讀同一個 `EquityTracker`。平倉、減倉、撤單不受私有流與風控鎖定限制（條件 60）。
  - `session_verdict` 存在 `Flowsurface` 層的記憶體 session，不是 modal 的 `Status`。
  - 本 session 沒測過 → 拒絕；`Rejected` → 拒絕；Live 必須是 `Usable`；Testnet 可為 `Unverified`（`is_usable`，`account.rs:171-176`）。
  - 每次送單都重新 `load_credentials`，比對 Keychain 與存檔的 venue、env、product，不一致就拒絕；簽名後立即釋放憑證。
  - 帳戶被編輯或刪除、Keychain 載入失敗、私有流 `AuthFailed`（`StreamOutput::InvalidateSession`）時，session verdict 立即作廢。
  - Binance Live 現貨另要求 `enableSpotAndMarginTrading == true`（欄位已在官方 wallet SDK 的 `get_api_key_permission_response.py:68` 核對）；合約要求 `enableFutures == true`。缺少或為 false → 該產品不可下單。
  - 驗證：涵蓋全部組合的表格式單元測試；手動改 `saved-state.json` 的 env 後，下單被拒。
- **58（a）**：合約下單時，builder 依**交易所回報的實際持倉模式**（存在 session）決定 `positionSide`／`posSide` 與是否帶 `reduceOnly`；實際模式未知 → 拒絕送出合約單。條件 28 的「不一致只警告」仍適用於顯示，但 builder 只看實際值。
  - 依據：依存檔值組參數，在 hedge 帳戶會送出 `reduceOnly` 或缺 `positionSide`，造成被拒或方向錯誤。
  - 驗證：golden 測試，one-way、hedge、未知三種模式各一組。

**K. 冪等**

- **59（a）**：每張單（包括 algo、order list 各腿、`attachAlgoOrds`）都帶 app 產生的 client id：字元 `[A-Za-z0-9]`、長度 ≤32，同時滿足 Binance 36 字元、OKX 32 字元，以及 Binance 現貨 FIX 規則。格式為角色前綴（`e`／`s`／`t`／`r` 等）加至少 96 bit 隨機 base62；不含帳戶 id 或個人資訊；不用每次啟動會歸零的序號。送出前依各 venue 正規式驗證。涵蓋欄位：`newClientOrderId`、`clientAlgoId`、`listClientOrderId`、`above/belowClientOrderId`、`clOrdId`、`algoClOrdId`、`attachAlgoClOrdId`。
  - 依據：沒有 client id，就無法在逾時後查詢這張單是否已成立。
  - 驗證：性質測試，產生 10⁵ 個 id，全部符合三家正規式且不重複。
- **60（a、c）**：`Unknown` 的處理：**絕不自動重送**。依 client id 查詢（Binance `GET /fapi/v1/order`、`GET /fapi/v1/algoOrder`、`GET /api/v3/order`、`GET /api/v3/orderList`；OKX `GET /trade/order`、`GET /trade/order-algo`），並在 backoff 下重試查詢，直到查到為止。超過時間窗（Binance 為 `timestamp + recvWindow` 加緩衝）仍查不到，UI 顯示「未成立（推定）」，由使用者**手動**重送，重送用新 client id 並重新確認。查詢期間，該 (帳戶, symbol) 的送單按鈕停用。OKX `expTime` 可縮短不確定時間窗（**未核對**）。
  - 依據：盲目重送會重複開倉。
  - 驗證：狀態機單元測試，模擬逾時 → 查到、逾時 → 查不到、查詢本身也逾時三種情境，斷言整個流程沒有任何送單呼叫。
- **61（c、f）**：防重複按：確認權杖只能用一次，第二個 `Confirm` 視為 no-op；請求進行中與 `Unknown` 未解決時，該 (帳戶, symbol, 動作) 的按鈕停用。這兩者是**唯一**的防線，**不得依賴交易所以 client id 重複來擋**：Binance 與 OKX 的 client id 只在掛單中唯一，前一筆成交或進入終態後就能重用（explorer 報告 1.4、1.5、1.7），立即成交的市價單照樣會被重複送出。
  - 驗證：在同一個 update 迴圈送入兩個 Confirm，只產生一個送單 Task。

**L. 下單正確性**

- **62（a）**：新增 `TradingRules`，由 exchangeInfo／instruments 的**原始字串**解析成十進位定點數（i128 與 scale，或經審查的 decimal crate；新依賴須符合條件 35）。欄位：tickSize、stepSize、minQty、maxQty、`MARKET_LOT_SIZE`、`MIN_NOTIONAL`／`NOTIONAL`、`PERCENT_PRICE`（**未核對**）、`TRAILING_DELTA`；OKX `tickSz`、`lotSz`、`minSz`、`ctVal`。不得使用 f32／f64、`MinTicksize`、`MinQtySize`。取整：數量一律向零取整；限價買單向下、賣單向上（避免越價成交）；觸發價取最近 tick。輸出不帶指數的十進位字串。規則缺少或過期 → 拒絕送出。標的精度超過內部表示能力 → 該標的不可交易。
  - 依據：`unit.rs:43-53` 會把 tick 四捨五入成 10 的次方（例如 0.5 變成 1）。
  - 驗證：golden 測試，tick 0.5、0.25、0.001、1e-9 與 step 0.001、1e-9 的取整結果；取整前後的往返檢查。
- **63（a、c）**：上限檢查：小於 minNotional 或 minQty、大於 maxQty（市價單用 `MARKET_LOT_SIZE`）一律拒絕；本機單筆上限（PRD 3.3）。防手誤：限價偏離標記價或最新價超過門檻（預設 5%，可設定）時，確認視窗顯示紅色警示。OKX 合約 `sz` 的單位（張）與等值幣量、名目價值，要在表單與確認視窗同時顯示（以 `ctVal` 換算）。
  - 依據：張數與幣量混淆，下單量可能放大 `1/ctVal` 倍。
  - 驗證：每項檢查各一個邊界 case。手動：OKX 模擬盤下 1 張單，核對畫面上的名目價值。
- **64（a）**：方向與模式一致，由純函式 builder 推導：hedge 模式**永遠不送 `reduceOnly`**；平多為 `SELL＋LONG`、平空為 `BUY＋SHORT`；保護單方向必須與持倉相反；`closePosition` 只能用於 `STOP_MARKET`／`TAKE_PROFIT_MARKET`，且不帶 `quantity`；OKX 同時帶 TP 與 SL 時，builder 只產生 `oco`，產生同時帶兩者的 `conditional` 時直接回錯；現貨保護賣單數量 ≤ 扣除手續費後的可用餘額。
  - 依據：`BF-TRADE:1393-1396,1420`；OKX 的 conditional 同帶 TP 與 SL 時只會執行 SL。
  - 驗證：golden 測試，one-way、hedge、OKX net、OKX long_short、現貨五種情境，每種含進場、止損、止盈、追蹤；反例：hedge 要求帶 `reduceOnly` 時回錯。
- **79（a、d）**：現貨保護單使用 `orderList/oco`、`orderList/otoco`，不用已標示 Deprecated 的 `/api/v3/order/oco`。`trailingDelta` 必須落在 `TRAILING_DELTA` filter 範圍內。現貨沒有 `reduceOnly`：止損賣單會賣出帳戶中任何來源的該資產，UI 要提示。
  - 依據：`BS/rest-api.md:3187,3303`。
  - 驗證：OCO 與 OTOCO 參數的 golden 測試；`trailingDelta` 超出範圍時被拒。

**M. LIVE 確認與一鍵操作**

- **69（c、f）**：LIVE 二次確認視窗：
  - 顯示 LIVE 標記、venue、env、帳戶別名、產品（現貨／USDⓈ-M／SWAP）、symbol、方向與 positionSide、類型、數量（兩種單位）、價格或觸發價、預估名目價值、reduceOnly 或 closePosition、將自動附掛的保護單。
  - **顯示的是 builder 取整後實際送出的值**，不是表單輸入值。
  - 確認按鈕沒有預設焦點；Enter、Space 與任何快捷鍵都不會觸發；視窗出現後 ≥ 700 ms 才啟用確認按鈕（防連點穿透）。
  - 確認權杖綁定請求參數的雜湊，參數改變時權杖作廢。
  - DOM 點價、拖曳改單（撤舊＋下新）、重試保護單同樣適用。
  - popout 視窗中的操作，確認視窗出現在同一個視窗。
  - 依據：使用者決定 7（Testnet 不需延遲與紅色標記）；避免第一下點擊後，第二下連點直接落在「確認」上。
  - 驗證：權杖與參數雜湊不符時不送出；啟用延遲前送入 Confirm 無效；鍵盤事件不產生 Confirm。手動：測試網依序走 Enter、Space、連點三種操作，都不能繞過確認。
- **70（c、e）**：一鍵撤全部、一鍵全平：確認視窗列出受影響的帳戶、訂單數、持倉數與總名目價值。全平時，one-way 用 `reduceOnly` 市價單；hedge 依 `positionSide` 分別平倉；OKX 用 `close-position` 或等效方式（端點**未核對**）。結果依條件 68 重新查詢後回報。
  - 驗證：同條件 68；另手動走一次 hedge 帳戶全平。

**N. 本機風控**

- **71（d、e）**：非點擊的送單路徑**只允許**三條：(1) 風控自動撤單與平倉；(2) 成交後自動附掛保護單；(3) 一張成交後撤另一張。三者都只能減倉、平倉或撤單，**不得增加曝險**，並寫入本機日誌。builder 對這三條路徑強制套用 reduce／close 語意。三者與「每次 LIVE 送出都要確認」的關係見使用者決定 2。
  - 依據：「只能手動下單」的界線要明確，否則日後新增的自動功能會繞過確認。
  - 驗證：code review 列出所有呼叫送單函式的地方，只能是確認流程與這三條路徑。單元測試：這三條路徑產生進場（增倉）單時回錯。
- **72（e、c、f）**：單一閘門：所有送單（面板、DOM、拖曳、自動附掛、重試）都經過 `submit(OrderIntent)`，依序執行 F2 閘門 → 風控 → 確認 → 送出。送單函式只接受 `ApprovedOrder`，它不實作 Clone，建構子為私有，只能由閘門產生。風控鎖定時，新進場被拒，減倉、平倉、撤單不受影響。
  - 依據：避免 DOM 點價或拖曳繞過風控。
  - 驗證：編譯層，`ApprovedOrder` 在閘門模組外無法建構（加 `compile_fail` doctest）；風控鎖定時各種 intent 的放行結果。
  - 排程：建議提前併入 a3 一起合併（見切分與順序）。
- **73（e）**：權益口徑在 UI 寫明：Binance 合約用 `totalMarginBalance`（錢包餘額加未實現損益），OKX 用 `totalEq`，現貨以標記價估值（實際欄位**未核對**，(e) 依測試網回應確認）。權益過期或未知（斷線、REST 失敗）時阻擋新進場，但**不觸發自動平倉**。誤觸發防護：連續 N 次觀測都超過門檻才觸發；轉入轉出（`ACCOUNT_UPDATE` 的 reason 欄）要校正高點；自動平倉預設關閉，LIVE 需每帳戶明確開啟。當日起點要指定時區。高點只存記憶體，重啟後重設，UI 要註明。
  - 依據：錯誤或過期的權益，可能造成不該發生的全平。
  - 驗證：純函式單元測試，涵蓋資料缺漏、轉帳、短暫跳動、跨日。
- **74（e）**：風控日誌寫在資料目錄（`FLOWSURFACE_DATA_PATH` 下），檔案權限 0600，大小有上限並輪替。內容只含時間、帳戶別名與 uuid、venue、env、symbol、動作、原因、數值；不含 key、secret、passphrase、signature、listenKey、交易所 msg（沿用 F11）。路徑由程式組合，不使用外部字串作為檔名。
  - 驗證：寫入含假 secret 的事件後讀回，斷言不含 secret；確認檔案權限。

**O. 指標與交易隔離**

- **75（a、c）**：(1) `indicator-engine` 不得依賴 `exchange`、`reqwest`、`tokio`、`hyper`；(2) 交易 API 放在 `exchange::trade`，送單函式要求條件 72 的 `ApprovedOrder`；`data/src/chart/kline/indicators/` 與 `indicator-engine/` 不得引用 `exchange::trade`；(3) `.ind` 語言不新增任何具副作用的 intrinsic（下單、警報、網路、檔案）；runtime 輸出只有繪圖與數值；(4) 下單表單不得自動帶入指標值（使用者手動點選採用不在此限）。
  - 依據：`data` 依賴 `exchange`，crate 邊界本身不構成隔離（摘要第 8 點）。
  - 驗證：`cargo tree -p flowsurface-indicator-engine -e normal | grep -E 'flowsurface-exchange|reqwest|tokio|hyper'` 必須沒有結果；`grep -rn 'exchange::trade\|ApprovedOrder' data/src/chart indicator-engine/src` 必須沒有結果。

**P. 時間同步**

- **76（a）**：下單請求 `recvWindow ≤ 5000`；時間差在啟動時、每 N 分鐘、睡眠喚醒後重新校正。`-1021`、OKX `50102`／`50112` 視為 `Rejected(ClockSkew)`（請求在處理前就被拒，不會成立），**不自動重送**，UI 提示校時；使用者手動重送時重新產生時間戳與簽名。OKX REST 用 ISO 毫秒時間戳，WS login 用秒。
  - 依據：條件 33；`binance_clock_offset` 失敗時回 0（R6）。
  - 驗證：`-1021` 對應到 `Rejected(ClockSkew)`，沒有任何重送呼叫。

**Q. 錯誤訊息**

- **77（a、c）**：交易所 `msg` 只顯示在 UI，依字元（`chars().take`）截斷，不寫入 log（沿用 F11）。`code` 限制為 `[-0-9A-Za-z]` 且 ≤16 字元（處理 F12）。OKX 同時檢查頂層 `code` 與每筆 `data[].sCode`，逐筆判定（R5）。未知錯誤碼一律視為 `Rejected`（前提是交易所明確拒絕）；結果不明的依條件 39 視為 `Unknown`。新程式碼不得使用 `String::truncate`（R7）。
  - 依據：交易所的 msg 可能回顯 IP 或請求內容。
  - 驗證：多位元組 msg 截斷不 panic；不合法的 `code` 被替換；OKX `code="0"` 而 `sCode="51008"` → `Rejected`。

**R. 序列化與現貨**

- **78（d、f）**：`AccountMeta` 新欄位全部加 `serde(default)`，不含機密。預設值：自動附掛關閉、自動平倉關閉；`reduceOnly` 只在 one-way 預設開啟，hedge 顯示「由 positionSide 控制」並停用。訂單、持倉、風控高點等執行期狀態，以及圖表上的訂單線，都不得寫入 `saved-state.json`，也不得放進 `layout::drawings::Library`。
  - 依據：條件 36；檔案可能被人修改後讀回。
  - 驗證：擴充條件 36 的測試，序列化整個 `State`，斷言不含假 secret、listenKey 或訂單 id；舊版存檔仍能載入。
- **80（f）**：DOM 點價要用修飾鍵加點擊（不能單擊送單），保留既有的捲動重置行為（依 explorer 報告，`ladder.rs:242-244`）。由列座標反推價格後，取整到**交易所 tick**，不是顯示分組用的 `PriceStep`。
  - 拖曳改單：放開後先確認原單仍為掛單中。保護單在**交易所允許時**採「先下新單、確認成立後再撤舊單」，避免中間出現無保護的空窗。交易所不允許兩張並存時，改用「先撤後下」，並在空窗期間顯示條件 65 的警示，新單失敗就是裸倉。不允許並存的情況：OKX 每個持倉只允許一張 `closeFraction` 全平 TPSL；現貨 OCO 會鎖住餘額，第二張會被拒；Binance `closePosition` 是否允許並存**未核對**。
  - 進場限價單採「先撤、撤成功才下新」，撤單失敗（例如已成交）就中止。
  - 與 PRD 3.4 原本的「撤舊單再下新單」不同，見使用者決定 9。
  - 驗證：純函式測試，座標 → 價格 → 取整。狀態機測試：舊單已觸發時中止改單。手動：測試網拖曳止損線。
- **81（b、d）**：進場單在 app 關閉或私有流斷線期間成交時：Binance 合約與現貨（非 OTOCO）的保護單由 app 在成交後送出，附掛意圖只存在記憶體（條件 78），app 不在線時保護單永遠不會送出。要求：
  1. 啟動時與每次重新同步後，比對所有持倉與掛著的保護單。**沒有任何保護單的持倉**一律顯示條件 65 的紅色橫幅，並提供「掛上預設保護單」（LIVE 需確認）與「平倉」。
  2. 進場確認視窗明示：「Binance 合約的自動附掛只在 app 執行中有效；若要離線也受保護，請改用 OKX `attachAlgoOrds` 或現貨 OTOCO，或進場後立即手動掛保護單」。
  3. 掛著進場限價單時關閉 app，要跳出提醒。
  - 依據：限價進場單常在 app 關閉後才成交，當下沒有任何東西會替它掛保護單。
  - 驗證：純函式測試，給定持倉與掛單清單，找出無保護的持倉（hedge 兩邊分開計算）。手動：在測試網掛進場限價單、關閉 app、讓它成交後重開，確認橫幅出現。

**保護單、撤單與冪等（65–68）**

- **65（d）**：保護單失敗即裸倉。成交事件 → 送出保護單。保護單 `Rejected`、`Unknown` 未解決、或遇 429 時，顯示**常駐紅色橫幅**與系統通知，並提供「重試保護單」（手動、新 client id、LIVE 需確認）與「平倉」，直到持倉有保護或已平倉為止。不得只寫 log。OKX `attachAlgoOrds` 在部分成交期間沒有保護，UI 標示「部分成交，保護單尚未生效」。
  - 依據：本專案最大的資金風險。
  - 驗證：狀態機測試，成交 → 保護單失敗 → 橫幅出現 → 重試成功 → 橫幅消失。手動：測試網用不合法的觸發價，讓保護單被拒一次。
- **66（d）**：Binance 合約兩張獨立保護單：收到其中一張的 `ALGO_UPDATE` 觸發或成交事件後，撤銷另一張（經 limiter 的優先級）。app 離線的風險要在 UI 與文件說明：one-way 用 `closePosition` 或 `reduceOnly`，不會反向開倉；hedge 下 `positionSide` 決定只能平哪一邊，不會反向開倉，但**殘留的那張可能作用在之後同方向的新持倉上**。啟動或重新同步時，偵測「沒有對應持倉的保護單」，並提示撤銷。`closePosition=true` 在沒有持倉時觸發的行為**未核對**。
  - 驗證：狀態機測試，一張成交 → 撤另一張；離線後重新同步 → 偵測孤兒保護單。手動：測試網關閉 app，觸發止損後重開。
- **67（a）**：不能下出撤不掉的單。每種支援的訂單類型，都必須同時具備已實作且測試過的「撤單」與「查單」路徑，才能開放下單。OKX `move_order_stop` 在核對並於模擬盤實測撤單端點前**不開放**。
  - 依據：`cancel-algos` 不含追蹤止損（explorer 報告 1.5）。
  - 驗證：合併前核對「類型 × {下單, 撤單, 查單}」矩陣全部打勾，且都有測試網紀錄。
- **68（a、e）**：「撤全部」涵蓋所有類別：Binance 合約，每個有掛單的 symbol 都撤 `allOpenOrders` 與 `algoOpenOrders`；現貨撤 open orders 與 order list；OKX 撤一般單、`cancel-algos`（conditional、oco）與追蹤止損端點。完成後重新查詢，**掛單數為零才回報成功**；否則列出剩下哪些單。
  - 依據：`allOpenOrders` 不含 algo 單；以為已全撤，其實止損單還在。
  - 驗證：假查詢結果模擬部分殘留，斷言回報「部分失敗」並列出剩餘項目。手動：測試網掛齊各類單後撤全部。

## M5 切分與順序

- **a1（傳輸、白名單、limiter、F2 閘門）**：條件 38–44、53–58、57；不含任何下單端點。
- **a2（TradingRules 與 builder）**：條件 59、62–64、79；只產生參數，不送出。
- **a3（送出、撤單、查詢、冪等流程）**：條件 39、60、67、68、76、77；建議把條件 72 的 `ApprovedOrder` 閘門一起併入，讓 c 從第一天起就只能經過閘門送單。
- 順序：a1 → a2 → a3 →（b 與 e 平行）→ c → d → f。
- a1 拆分的理由：現行 (a) 會同時改動 limiter（影響行情）與 1034 行的 `account.rs`，審查不易。
- **LIVE 送單路徑在 (e) 合併前不在 main 上開放**：例如 env 為 Live 時由閘門直接拒絕，(e) 合併時移除。這是避免 main 上出現「能送 LIVE 但還沒有風控閘門」的中間版本，見使用者決定 8。
- OKX 追蹤止損放在 a3 之後的補丁，前提是撤單端點已核對（條件 67）。

## M5 每段合併前的安全驗收清單

共同項目（每段都要做）：

- `cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets --all-features -- -D warnings`。
- `git diff main | grep -n 'danger_'` 結果為空。
- 新增的 Debug 遮罩測試通過。
- diff 中沒有任何真實金鑰（以 `grep -nE '[A-Za-z0-9]{40,}'` 人工檢視）。
- `Cargo.lock` 新依賴符合條件 35。

**a（a1／a2／a3）**

- [ ] 38：log 與錯誤不含 body、query、signature（測試）
- [ ] 39：三態對應表測試；5xx、2xx 不可解析、`-1006`／`-1007`／`50004`／`51149` → `Unknown`
- [ ] 40／41：(venue, env, product, scheme) 白名單表格測試；`ws://`、8443、跨環境組合被拒
- [ ] 42：proxy 快照比較；Broken proxy 拒絕連線；手動驗證不直連（同時完成 F9）
- [ ] 44：簽名字串等於送出內容的 golden 測試
- [ ] 53–56：limiter 共用、優先級、429 與 418 測試；行情 fetch 回歸
- [ ] 57／58：閘門表格測試；改存檔 env 後被拒
- [ ] 59／60：client id 性質測試；`Unknown` 流程不重送
- [ ] 62–64、79：取整與 builder golden 測試（含 hedge 不帶 `reduceOnly`、OKX 必須用 oco）
- [ ] 67：類型 × {下單, 撤單, 查單} 矩陣
- [ ] 72（若提前）：`ApprovedOrder` 的 `compile_fail` 測試
- [ ] 75：`cargo tree` 檢查 indicator-engine 依賴
- [ ] 76／77：`-1021` 不重送；OKX `sCode` 判定；msg 不入 log

**b**

- [ ] 40：私有 WS 只能透過 `wss_endpoint` 建立
- [ ] 42：每次連線與重連都檢查 proxy 快照
- [ ] 45／46：listenKey、認證 frame 不出現在 log、Debug、Hash（測試加 code review）
- [ ] 47：OKX WS 簽名向量；認證失敗不重連
- [ ] 48：現貨訂閱失敗時 fail closed
- [ ] 49／50／51：listenKey 生命週期、backoff、斷線狀態與動作矩陣測試
- [ ] 52：變形輸入不 panic
- [ ] 手動：測試網斷網 30 秒並恢復，確認重新同步完成前不能新進場

**c**

- [ ] 57：閘門讀取記憶體 session，不讀 modal `Status`
- [ ] 61：雙 Confirm 只送出一次
- [ ] 69／70：確認視窗內容是 builder 的輸出；延遲啟用；鍵盤無法繞過（單元測試加手動）
- [ ] 51：斷線時 UI 狀態與按鈕停用
- [ ] 新的 `Message` variant 不帶憑證或 listenKey（Debug 測試）
- [ ] 手動：測試網下單、撤單、LIVE 確認視窗內容逐欄核對

**d**

- [ ] 65：裸倉橫幅狀態機測試，加測試網手動觸發一次
- [ ] 66：撤另一張、孤兒保護單偵測
- [ ] 81：啟動與重新同步時偵測無保護持倉；確認視窗註明自動附掛只在 app 執行中有效
- [ ] 71：自動路徑只允許減倉、平倉、撤單
- [ ] 78：序列化相容與無機密測試

**e**

- [ ] 72：單一閘門；風控鎖定時減倉仍放行
- [ ] 73：權益過期 → 阻擋新進場、不自動平倉；轉帳校正；自動平倉預設關閉
- [ ] 74：日誌無機密、權限 0600、大小上限
- [ ] 68／70：撤全部、全平後重新查詢才回報結果
- [ ] 移除「Live 不開放」的暫時限制（採用切分與順序的做法時）

**f**

- [ ] 80：修飾鍵加點擊；取整到交易所 tick；保護單先下新再撤舊
- [ ] 69：點價與拖曳在 LIVE 下都經過確認
- [ ] 72：點價與拖曳經過同一個閘門（code review：f 不直接呼叫送單函式）
- [ ] 78：訂單線不寫入 `Library` 與 `saved-state.json`
- [ ] 手動：Windows 分支不受影響；popout 視窗中的確認視窗出現在正確的視窗

## M5 使用者決定（2026-11-10）

1. **Binance 帳戶加「產品」欄位**（現貨／USDⓈ-M）。測試網兩個產品各建一個帳戶（條件 41）。
2. **三條自動路徑採「預先授權」**（條件 71）：進場確認視窗列出之後自動附掛的保護單與互撤行為，確認一次即涵蓋。風控自動平倉預設關閉，LIVE 帳戶要在設定中明確開啟。自動路徑只能減倉、平倉、撤單。
3. **Binance 現貨私有流在測試網實測確認 HMAC 可用前，現貨交易停用**（fail closed，條件 48）。
4. **OKX 追蹤止損在撤單端點核對並實測前不開放**（條件 67）。
5. **hedge 帳戶的保護單只提供全平型**（`closePosition`／`closeFraction`）。
6. **防手誤門檻預設 5%**（條件 63）；**limiter 每桶保留 20% 給撤單、平倉、保護單**（條件 55）。
7. **Testnet 帳戶走同一確認流程**，但沒有延遲與紅色 LIVE 標記（條件 69）。
8. **本機風控（段 e）合併前，main 上不開放 LIVE 送單**（開發順序限制，不改變「LIVE 一開始就能送單」的產品決定；見切分與順序）。
9. **拖曳改保護單**（條件 80）：交易所允許時「先下新、再撤舊」；不允許時先撤後下，並顯示裸倉警示。

## M5 前必做

- **F1（中）**：Binance 簽名請求（`exchange/src/account.rs` 的 `send_get`）沒有與公開 `/fapi` 請求共用 limiter。M5 下單與查倉會持續送出簽名請求，必須共用同一個 limiter 實例，並在每次送出前取得配額。
- **F2（中）**：判定結果目前只存在 modal 的 `Status`。M5 下單前必須讀取記憶體中的判定，確認 `is_usable(env, verdict)`。不得從 `saved-state.json` 讀取，也不得以「沒有測試過」當成預設可用。下單時要重新呼叫 `load_credentials`（會比對 venue 與 env），不要留存 Test 時取得的 `ApiCredentials` 重複使用。
- **F10 延伸**：M4 只在 Test 前檢查 proxy 是否變更（`signed_client_for_current_proxy`，現回傳 `SignedHttp`），變更後要求重啟。M5 必須把同一檢查延伸到每個簽名請求與私有 WebSocket。

## 其他後續項目

- F3、F4、F5、F6、F7、F10、F11：已在 M4 修正。
- F8（資訊）：Binance Demo Trading 金鑰（`demo-fapi.binance.com`）目前不支援，屬於 fail closed。日後支援時須新增環境並配一份專屬 host 白名單，不得放寬現有白名單。
- F9（資訊）：`apply_proxy_strict` 尚無單元測試。須實際手動驗證：設定無效 proxy 後按 Test，應拒絕送出，不得改為直連。
- F12（低）：`exchange/src/account.rs` 中字串型態的 `code` 未截斷，會寫進 log 與 UI。建議截斷，或只接受 `[-0-9A-Za-z]` 且長度有限的字串。
- F13（低）：`Form` 未實作 `Drop`，程式結束時未歸零。建議加上 `impl Drop`，呼叫 `clear_secrets()`。
- F14（資訊）：`sanitized()` 丟棄的帳戶，其 Keychain 項目會成為孤兒，UI 無法刪除。可考慮載入時寫一筆 warn log（只記 venue 與筆數）。

## Binance endpoint 已核對來源

以下欄位與 endpoint 已對照 Binance 官方 connector 原始碼核對（來源為 GitHub，未直接查閱 developers.binance.com）。

| 項目 | 結果 |
|---|---|
| `GET /sapi/v1/account/apiRestrictions`，base `https://api.binance.com` | 已核對。來源：`binance/binance-connector-python`（master）的 `clients/wallet/src/binance_sdk_wallet/rest_api/api/account_api.py`（`get_api_key_permission`）；`WALLET_REST_API_PROD_URL` 位於 `common/src/binance_common/constants.py` |
| 欄位 `enableWithdrawals`、`ipRestrict`、`enableInternalTransfer`、`permitsUniversalTransfer`、`enableFutures` | 已核對。來源：同 repo 的 `.../models/get_api_key_permission_response.py`，型別皆為 `Optional[StrictBool]`，與 `as_bool()` 取值方式一致 |
| `GET /fapi/v1/positionSide/dual` → `dualSidePosition`（bool） | 已核對。來源：`binance/binance-futures-connector-python`（main）的 `binance/um_futures/account.py`（`get_position_mode`）；model 位於 `binance-connector-python` 的 `.../models/get_current_position_mode_response.py` |
| `GET /fapi/v1/time` → `serverTime`（int） | 已核對。來源：`binance/binance-futures-connector-python` 的 `binance/um_futures/market.py`（`time()`）；model 位於 `binance-connector-python` 的 `.../models/check_server_time_response.py` |
| futures testnet host `https://testnet.binancefuture.com` | 已核對。來源：`binance-connector-python` 的 `common/src/binance_common/constants.py`（`DERIVATIVES_TRADING_USDS_FUTURES_REST_API_TESTNET_URL`） |
| testnet 沒有 `/sapi` | 未直接核對。constants 中沒有 wallet 的 testnet URL，間接支持此說法。即使有誤，實作也只會判為 `Unverified`（較保守） |

轉帳旗標語意（依官方 model 的 description）：`enableInternalTransfer` 為母子帳戶之間轉帳；`permitsUniversalTransfer` 為使用 universal transfer API。兩者都只能在使用者自己的帳戶之間移動資金，因此只發警告、不拒絕。轉給他人屬於 `enableWithdrawals` 的範圍，該欄位需先綁 IP。

補充：同一份 constants 另有 `https://demo-fapi.binance.com`（Demo Trading）。Demo Trading 產生的金鑰不能用在 `testnet.binancefuture.com`，按 Test 會得到 `Rejected`，失敗方向是安全的。

## 既有問題（未修）

以下問題不屬於 M4 範圍，M4 審查時發現但未修改：

- `src/connector/client.rs` 的 `server_http`（中）：設定 `danger_accept_invalid_certs(true)`，並在每個請求加上 `Authorization: Bearer <token>`。網路上的 MITM 可取得 server token，也可竄改回傳的交易資料。這違反「憑證驗證必須維持開啟」的原則。
- `exchange/src/error.rs` 的 `String::truncate(177)`（低到中）：第 177 個位元組不在字元邊界時會 panic。多位元組的錯誤訊息（例如 zh-TW Windows 的系統訊息）可能觸發。
- 既有 proxy fail open（高，既有路徑）：`exchange/src/adapter/proxy.rs` 的 `try_apply_proxy` 在 proxy URL 無效時回傳不帶 proxy 的 builder，等於改為直連。簽名請求已改用 `apply_proxy_strict`，不受影響；其他既有請求路徑仍有此行為，M4 未修改。
- `ServerClient` 與 `Network` 使用 `derive(Debug)`（低到中），會把 `auth_token` 或 `server_auth_token` 以明文輸出。目前未找到會輸出它們的 log，但屬潛在風險。
- `src/connector/client.rs` 的 `text()`、`bytes()`（低到中）：伺服器回應沒有大小限制，非 2xx 時還會把整個回應寫進 log。
- `src/main.rs` 的 `Message`，以及 `src/modal/network_editor.rs` 的 `ProxyMsg::PasswordChanged`、`FetchMsg::ServerAuthTokenChanged`，使用 `derive(Debug)`（低）。以 `debug` feature 建置時，會把 Message 的前 49 個字元送到 comet beacon，含明文輸入時會洩漏部分 secret。


## c2a 補充：Unknown 紀錄、握手與重複 key

- **journal 寫入順序**：進場單先寫入（write-ahead）journal 再送出，已知結果後才清除；寫入失敗的新進場被拒。
- **B5 分支**：非進場單（平倉、取消、保護單）journal 寫入失敗時仍送出，並以 `Submitted.journal_failed`／`BulkReport.journal_failed` 回報，GUI 應顯示常駐警告。
- **`block_all_entries` 語意**：載入 Unknown store 失敗時，所有帳戶的新進場封鎖到重啟為止；平倉、取消與保護單不受影響。現貨部位只能靠保護單退出。
- **丟棄無效紀錄**：只有 client id 無效的紀錄可丟棄。流程為 `begin_discard` → 使用者輸入確認句 → `DiscardToken` → `UnknownStore::discard_invalid`，並寫入風控日誌 `unknown_discarded_invalid_record`。丟棄後帳戶仍維持封鎖到重啟。
- **L-4 握手**：私有 WS 握手整體期限 15 秒、單次讀取 10 秒；握手期間緩衝的事件上限 512，超過視為網路錯誤。未被要求的 channel 確認不計入。
- **重複 API key**：`ApiCredentials::same_key_as` 只比對 key；GUI 儲存前以 `check_key_not_bound` 檢查同 venue／env／product 的其他帳戶，Keychain 讀取失敗視為無法驗證而拒絕。driver 以程序內 `key_claims`（SHA-256 摘要為 key）防止兩個帳戶共用同一把 key 的 listenKey。
- **Unknown store I/O**：拒絕 symlink 與非一般檔案（FIFO），rename 後 fsync 目錄；GUI 使用 `*_async`（`spawn_blocking`）包裝。殘餘風險：檢查後才被換成 FIFO 仍可能卡住（未用 `O_NONBLOCK`）。
- **冪等**：同一紀錄重複 `lock_from_record` 只需一次釋放。
