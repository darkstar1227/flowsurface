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
- **51（b、c）**：每個帳戶的私有流狀態為 `Connected`／`Reconnecting`／`Resyncing`／`AuthFailed`／`Unavailable`，在 UI 明顯顯示。狀態不是「`Connected` 且已重新同步」時：委託與持倉列表標為「可能過期」；**禁止新進場單與拖曳改單**；撤單、平倉、撤全部、全平仍允許，**唯一例外是 `AuthFailed`：依條件 57，`approve` 拒絕所有單，含撤單、撤全部、平倉、減倉、全平與自動平倉（憑證被拒代表 session 作廢）**；「一張成交撤另一張」暫停，重連後先以 REST 同步再補做。重新同步的端點：Binance 合約 `positionRisk v3`、`openOrders`、`openAlgoOrders`；現貨 `openOrders`、`openOrderList`、`account`；OKX `positions`、`orders-pending`、`orders-algo-pending`（`orders-algo` 不推快照）。睡眠喚醒視同斷線。
  - 依據：依過期資料下單或拖曳，會撤錯單，或在已平倉時又掛出新單。
  - `AuthFailed` 的判定採**允許清單**（c2b 前置，review S1-a）：誤判會把「擋進場」放大成「擋出場」，所以只有確定代表憑證被拒的回應才算。
    - Binance：錯誤碼 `-2014`、`-2015`、`-1022`、`-2008`（含帶這些碼的 401／403）。單純的 403（WAF 限流）、沒有錯誤碼的 401／403、其他碼，一律當暫時性錯誤（`Network`，走 backoff）。
    - OKX（依官方錯誤碼表核對，REST 501xx：https://www.okx.com/docs-v5/en/#error-code-rest-api-public ，WebSocket 600xx：https://www.okx.com/docs-v5/en/#error-code-websocket-public ）：`50100`（API 被凍結）、`50101`（key 與環境不符）、`50105`（passphrase 錯）、`50111`（key 錯）、`50113`（簽名錯）、`50119`（key 不存在）；WS 登入 `60005`、`60007`、`60024`、`60032`。刻意不列：`50110`（拒絕的是出口 IP，不是憑證，出口恢復後應自行重連）、`50114`、`50120`（只針對單一功能）、`50103`／`50104`／`50106`／`50107`（本機 header 錯誤）、`60009`、`63999`、`60011`、`60026`、`60031`，一律當暫時性錯誤。時間戳類的碼（`50102`、`50112`、`60004`、`60006`）仍是 `ClockSkew`，不是 `AuthFailed`。未驗證：真實 OKX 憑證錯誤時 WS 登入是回上列具體碼還是泛用的 `60009`，列入測試網手動驗證。
    - 實作與表格測試：`exchange/src/trade/stream/driver.rs` 的 `classify_rest`、`login_step`。
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
  1. 啟動時與每次重新同步後，比對所有持倉與掛著的保護單。**沒有任何保護單的持倉**一律顯示條件 65 的紅色橫幅，並提供「掛上預設保護單」（LIVE 需確認）與「平倉」。**（d5 已實作：橫幅、「Add a stop-loss」（必經確認視窗，新 client id）、「Flatten」、macOS 通知；「預設保護單」以重試止損的邏輯實作，用帳戶預設的止損設定。需 testnet／手動：限價進場、關閉 app、成交、重開，在主視窗與 popout 各驗一次。）**
  2. 進場確認視窗明示：「Binance 合約的自動附掛只在 app 執行中有效；若要離線也受保護，請改用 OKX `attachAlgoOrds` 或現貨 OTOCO，或進場後立即手動掛保護單」。
  3. 掛著進場限價單時關閉 app，要跳出提醒。
  - 依據：限價進場單常在 app 關閉後才成交，當下沒有任何東西會替它掛保護單。
  - 驗證：純函式測試，給定持倉與掛單清單，找出無保護的持倉（hedge 兩邊分開計算）。手動：在測試網掛進場限價單、關閉 app、讓它成交後重開，確認橫幅出現。

**保護單、撤單與冪等（65–68）**

- **65（d）**：保護單失敗即裸倉。成交事件 → 送出保護單。保護單 `Rejected`、`Unknown` 未解決、或遇 429 時，顯示**常駐紅色橫幅**與系統通知，並提供「重試保護單」（手動、新 client id、LIVE 需確認）與「平倉」，直到持倉有保護或已平倉為止。不得只寫 log。OKX `attachAlgoOrds` 在部分成交期間沒有保護，UI 標示「部分成交，保護單尚未生效」。
  - 依據：本專案最大的資金風險。
  - 驗證：狀態機測試，成交 → 保護單失敗 → 橫幅出現 → 重試成功 → 橫幅消失。手動：測試網用不合法的觸發價，讓保護單被拒一次。
- **66（d）**：Binance 合約兩張獨立保護單：收到其中一張的 `ALGO_UPDATE` 觸發或成交事件後，撤銷另一張（經 limiter 的優先級）。app 離線的風險要在 UI 與文件說明：one-way 用 `closePosition` 或 `reduceOnly`，不會反向開倉；hedge 下 `positionSide` 決定只能平哪一邊，不會反向開倉，但**殘留的那張可能作用在之後同方向的新持倉上**。啟動或重新同步時，偵測「沒有對應持倉的保護單」，並提示撤銷。`closePosition=true` 在沒有持倉時觸發的行為**未核對**。**（d5 已實作：孤兒偵測與逐單撤銷、殘留自動單提示；仍需 testnet：`closePosition=true` 在無持倉時的行為、`openAlgoOrders`／`orders-algo-pending` 的 client id 欄位名稱；互撤由 d3 實作。）**
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
- [ ] 78：訂單線不寫入 `Library` 與 `saved-state.json`（f2 已加測試 `order_lines_never_reach_the_saved_state`）
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

## c2b0-2 補充：給 GUI 接線的 exchange 縫

這些只加縫，不開放下單：`LIVE_SENDING_ENABLED` 仍為 false；host 白名單、https_only、redirect none、proxy fail closed 與 Keychain 都沒改。以下型別維持不可 `Clone`：`ClientId`、請求、`Prepared`、`ConfirmToken`、`Submitted`、`PendingUnknown`、`ApprovedOrder`、`EquityTracker`。

- **Send**：`RiskHook: Sealed + Sync`、`UnknownJournal: Sync`，所以 `submit`／`submit_auto`／`resolve_unknown`／`cancel_all`／`flatten`／`auto_close` 的 future 是 `Send`（測試 `every_send_path_future_is_send` 在編譯期檢查）。`G: SendGate` 沒有 `Sync` 約束，GUI 的 gate 實作（`SessionGate`）自己要是 `Sync`，future 才會是 `Send`。
- **`SharedRisk`**：第二個 sealed `RiskHook`，持有 `Arc<Mutex<EquityTracker>>` 與注入的時間來源，可 `Clone`（同一個 tracker）。判斷邏輯與 `RiskSnapshot` 共用同一個函式 `check_entry`，不會分歧（對照測試 `the_shared_hook_judges_exactly_like_the_snapshot`）。鎖只在各方法內取、不跨 `.await`（方法都是同步的）。**鎖 poisoned 時 fail closed**：進場回 `RiskRefusal::StateUnavailable`，平倉、撤單照常通過（條件 72）；會改狀態的方法回 `Err(RiskPoisoned)`，GUI 要顯示成風控狀態不可用。快照與事件走 `snapshot_applied`／`observe`／`set_positions`／`set_limits`，同一把鎖。`RiskRefusal` 新增 `StateUnavailable`（`data` 的風控日誌對應為 `state_unavailable`）。
- **`StreamCloser`**：`PrivateStream::closer()` 回傳可 `Clone` 的控制把手，`shutdown(&self)` 與 `PrivateStream::shutdown(self)` 語意相同（等 listenKey DELETE 回覆才回傳）。冪等：supervisor 只處理一次 shutdown，之後的呼叫（含同時發出的）不再送 DELETE。**殘餘風險／用法**：stream 已被 drop（subscription 已移除）或已結束時，closer 立即回傳且不關 listenKey，所以必須在移除 subscription **之前** `await closer.shutdown()`，再刪 Keychain。
- **送出值預覽與權杖雜湊（條件 69）**：`OrderIntent::preview() -> OrderPreview`（可 `Clone`、唯讀）列出 builder 取整後的實際參數：Binance 依送出順序，OKX 的 JSON body 攤平（`attachAlgoOrds[0].slTriggerPx`）並依名稱排序。client id 只顯示末四碼（`…AAAA`）；簽名、timestamp、憑證不在其中。`ConfirmToken::for_preview(&preview)` 綁定預覽的 SHA-256（涵蓋帳戶、產品、method、path、symbol 與完整參數，含完整 client id）；`approve` 內比對，不符回 `SubmitError::ConfirmationMismatch`，什麼都不送。雜湊欄位是 crate 私有，複製後被改過的預覽仍只能確認原本的請求。重建請求（連 client id 都會變）就需要重新確認。
- **取消全部／全平**：`ConfirmToken::for_bulk(account, BulkAction)`。權杖綁定帳戶與動作；訂單權杖不能用在 `cancel_all`／`flatten`，批次權杖不放行 `Adds`、也不能用在 `submit`（皆拒絕）。`ConfirmToken::user_confirmed()`（沒有綁定的權杖）已移除。
- **測試對應**：`the_preview_equals_the_form_the_binance_server_receives`、`the_preview_equals_the_json_the_okx_server_receives`（逐欄比對假伺服器收到的 body）、`a_token_for_another_quantity_is_refused_and_nothing_is_sent`、`a_token_for_another_client_id_or_another_kind_of_action_is_refused`、`a_closer_*`、`two_racing_closers_*`、`a_poisoned_shared_hook_*`。

## c2b-1 補充：GUI 接線（`src/trading.rs`）

c2b-1 只接私有 stream、風控狀態與常駐警示；**沒有任何下單路徑**（不呼叫 `submit`／`cancel_all`／`flatten`／`auto_close`，測試 `the_only_trade_calls_here_are_reads` 掃 `trading.rs` 原始碼（`.flatten(` 只允許無參數的 `Stream::flatten`）；`main.rs` 與 `exchange_accounts.rs` 以 grep 確認）。`LIVE_SENDING_ENABLED` 仍為 false。

- **開機順序**：先非同步讀 Unknown store，再依結果套用；讀取失敗 → `block_all_entries` 並顯示「日誌無法讀取」警示，不鎖任何紀錄；讀取成功 → 每筆紀錄 `lock_from_record`，個別失敗（壞 id）保留在警示中，不吞掉。開機報告套用前 `runs()` 為空，所以沒有任何 stream 會先於鎖定啟動。報告不含 client id。c2b-1 **不**跑 `resolve_unknown`（需要憑證，Unknown 面板在 c2b-3）；紀錄維持鎖定、唯讀顯示。
- **stream 生命週期**：只有本次 session Test 通過的帳戶才有 stream（Binance 現貨不建 stream，狀態 `Unavailable`，文字寫明未在 testnet 驗證）。重新 Test：以 generation+1 建新 stream，舊 stream 被丟棄（停止其所有 task），**不保證**舊 listenKey 已關閉——它不會被延長，約 60 分鐘後自行失效；只有刪除帳戶或停用才走 `closer.shutdown()` 主動關閉（S1-b：`AuthFailed` 之後重測可重新允許平倉）。舊 generation 遲到的輸出一律忽略（`key != 目前 key`）。`AuthFailed` 保留，不自動重建；`InvalidateSession` 使 session 判定失效並標記需重測。
- **風控 tracker**：每帳戶一個 `SharedRisk`，整個 app 生命週期不換（刪除失敗後的重測同樣沿用，這點在 c2b-2 的 N1 之後才成立：先前該路徑會 `remove` 而丟掉 tracker 與鎖）；stream 重啟、設定變更都只經 `set_limits`／`snapshot_applied`／`observe`，不替換 tracker（`a_risk_tracker_survives_a_new_stream`）。
- **刪帳戶順序**：先在期限內（15 秒）`closer.shutdown()`，再刪 Keychain；逾時仍刪（`delete_after_close` 測試）。刪除失敗 → session 判定作廢，stream 已關閉。
- **proxy**：stream 與淨值讀取每次連線都讀即時的 `proxy_cell`；與建 client 時的快照不符就拒絕連線／簽名（fail closed）。proxy 設定不可讀 → 初始為 `Broken`。
- **未計價部位 fail closed**：`Position` 沒有價格、此版本沒有價格來源，所以只要持有任何部位，名目價值就以哨兵 `?` 提供給 tracker → 有單一 symbol 上限時進場回 `PositionUnknown`。價格來源由後續階段補上。
- **淨值輪詢**：只輪詢 `Connected` 的帳戶，間隔為 `max_equity_age/3`（夾在 5–60 秒）；憑證只在 `spawn_blocking` 內短暫載入，失敗 → 無觀測（淨值會變陳舊並顯示警示）。
- **常駐警示**：主視窗與 popout 都顯示；內容含日誌／紀錄狀態、未測試、`AuthFailed`（全部下單含撤單與平倉暫停）、重連／重同步、風控鎖、風控狀態不可用、淨值陳舊、「未設任何風控上限」。

## c2b-2 補充：手動下單（票據、確認視窗、送出）

- **範圍**：側欄 modal（Settings → Order ticket），不新增 pane 類型、不動 layout 存檔格式，也不把任何訂單狀態寫進 `saved-state.json`。支援限價／市價進場、停損／停利（可選整倉）、Binance 期貨追蹤停損、撤銷單一掛單。**不含**全撤、全平、Unknown 釋放／丟棄（c2b-3）。
- **送單路徑只有一條**：`src/order_flow.rs::send` 是 GUI 裡唯一呼叫 `.submit(` 的地方，且只經 `SignedHttp`（不走 `server_http`）。手動單不使用 `submit_auto`／`AutoIntent::new`。`order_flow` 的測試掃描整個 `src/`：`.submit(` 只出現在該檔、不出現 `block_on`、`submit_auto`、`AutoIntent::new`；`for_preview(` 只出現在 `order_confirm.rs`，且參數是確認視窗存下的 preview，不是新呼叫 `.preview()` 的結果。
- **兩段式**：`prepare`（讀 Keychain → 建 client → 校時 → 抓規則 → builder → preview，只做讀取）與 `send`（確認後才重新讀 Keychain、含身分、重新判斷 proxy、建新 client）。兩步都不保留 client 或憑證。preview 的 intent 放在只能取出一次的 slot。
- **確認**：顯示 builder 實際送出的欄位；LIVE 有紅色 LIVE 標記與 700 ms 延遲，testnet 無延遲、無 LIVE 標記；單次有效、無預設焦點、無文字輸入、無鍵盤路徑（Escape 只取消）。限價偏離市價超過 5% 顯示紅色警告，沒有報價就明說沒有，不略過。OKX 顯示張數與換算後的基礎幣數量。確認視窗另顯示帳戶別名、venue、產品、環境（取自預覽所屬的帳戶，不是票據目前選的）與預估名目價值（無參考價時寫 unknown）或「Reduces / closes only」。準備預覽期間票據表單凍結，已開的確認視窗不會被新的預覽或自動重新預覽覆蓋（M1、M2）。
- **`LIVE_SENDING_ENABLED` 仍為 false**：LIVE 帳戶在票據上顯示 "Live trading is not open yet"，不送出；閘門本身仍會拒絕。
- **N4**：`trading::boot()` 與其他啟動工作並行（`Task::batch`），但在 boot 報告套用前，準備與送出都被拒絕（票據的 `blocked_reason` 與確認時各檢查一次）。
- **結果**：Unknown 時只做查詢（`resolve_unknown`），絕不重送；找到只說「交易所有這張單」，不說成交；查無只說「推定未下單」；仍未知則該 (帳戶, symbol) 維持鎖定直到重啟。`RulesExpired` 會自動重新 preview，需再按一次確認，不當成失敗。交易所 `msg` 以 `chars().take` 截斷、只顯示不記錄；摘要只帶 client id 最後四碼。
- **部位估值**：持有部位仍 fail closed（`PositionUnknown`），原因：報價沒有時間戳、OKX 需要 ctVal，無法保證估值新鮮。空倉帳戶則在 stream 為 Connected、已收過快照時，每次淨值輪詢重新蓋一次空帳本時間戳，避免帳本陳舊而擋下進場。
- **參考價**：來自行情表的 `mark_price`（Binance 與 OKX 實際上是最新成交價）。它不只用於 5% 偏離，也用於市價單的名目價值（影響 minNotional 檢查與本機單筆／symbol 上限），以及保護單觸發價方向檢查（沒有參考價就拒絕）。它沒有時間戳，連線失敗時會保留舊值；Live 開放前必須改成 `prepare` 內以 REST 讀取帶時間戳的標記價，過期就拒絕。
- **L4**：確認刪除帳戶的當下就讓該帳戶的 session 驗證失效，刪除進行中不能再送單。L7：沒有 runtime 時顯示「請重新 Test」的文案。
- **L2**：測試後 proxy 設定改變，常駐警示顯示「請重新測試」。N2：刪除中的帳戶不可測試，遲到的測試結果丟棄。

## c2b-3 補充：Unknown 解除／丟棄、撤全部、全平（`src/account_flow.rs`、`src/modal/{bulk_confirm,account_panel}.rs`）

這一段更新前面幾段的舊說法：Unknown 的 (帳戶, symbol) 不再「只能等重啟」——使用者可手動解除；開機已存的紀錄在該帳戶本 session Test 通過後會被查詢（c2b-1 寫的「不跑 `resolve_unknown`」只適用於 c2b-1）。`LIVE_SENDING_ENABLED` 仍為 false：LIVE 帳戶的撤全部／全平按鈕與票據一樣被 `blocked_reason` 擋下。

- **入口**：不新增 `Menu` 變體、不動存檔格式。帳戶區塊（`AccountPanel`）接在下單票 modal 下方，顯示所選帳戶的訂單／持倉（唯讀，來自 c2b-1 的私有 stream 快照，`lists_stale` 為真時顯示「可能過期」橫幅）、撤全部、全平（**任何 venue 的現貨都隱藏**，`flatten_offered`）、風控鎖、自動平倉設定（只顯示狀態，不提供開關，也沒有自動平倉）、Unknown 清單。保護單標示與裸倉橫幅留給 d。
- **bulk 權杖**：`ConfirmToken::for_bulk` 只出現在 `bulk_confirm.rs::PendingBulk::confirm`，參數是視窗開啟時存下的帳戶與動作（`for_bulk(self.account_key, action)`（`action` 取自視窗存下的 `Kind::Bulk(action)`）；同一行，掃描測試比對）。確認處理器 `confirm_bulk` 會重新找帳戶、重跑閘門（含「刪除中」、「已有 bulk 在跑」、風控 runtime 是否存在）後才取權杖，權杖只用一次。`for_preview(`／`for_bulk(` 的掃描測試：`the_tokens_come_from_the_stored_values_in_the_confirmation_files_only`。
- **確認視窗內容**（`BulkPreview::lines`）：帳戶別名、venue、產品、環境（LIVE 紅標、testnet 標 TESTNET）、訂單數、持倉數、symbol 清單；**名目價值寫「unknown」，不寫 0**（開放訂單沒有價格、此版本沒有持倉標記價）；沒有東西可做時顯示說明且 Confirm 按不下去。準備階段是新的 REST 讀取（`prepare_bulk`），不是 D3 列表用的快照快取。
- **一次一個確認視窗**：`order_confirm` 與 `bulk_confirm` 共用 M2 規則（`confirm_open()`）：已開的視窗不被任何背景結果取代（遲到的 bulk 預覽被丟棄並提示）；Escape、離開票據、`discard_order` 都同時關閉兩者。bulk 執行或準備期間，單筆下單的 review／confirm 被拒絕。
- **R10**：`BulkError::ConfirmationMismatch`（新增於 `exchange/src/trade/bulk.rs`，取代先前借用的 `Trade(Unsupported)`）；GUI 顯示專門文案 `BULK_MISMATCH`，不說「不支援」。`send_tests` 的四個斷言已改。
- **`BulkReport` 各欄位**都有文案，由表格測試 `every_bulk_report_field_has_a_text` 鎖住：`sent`、`failed`、`remaining`（含 symbol）、`reread_failed`（結果未知，請重新整理）、`unknown_algo_types`、`journal_failed`（同時 `set_journal_failed` 觸發常駐警示）、Unknown 查詢結果（`found` 不說成交、`not_placed`、`unresolved`）。仍未知的 symbol 以 `finish_send(.., true)` 標成 `Unresolved`，保持鎖定。結果以事後新讀取為準，不以請求數為準。`send_bulk` **刻意不加整體逾時**：請求送出後丟棄會失去報告；但第一個請求之前的步驟（Keychain 讀取、校時、`fetch_rules`）包在 `within(PREPARE_TIMEOUT)`（`bulk_setup_within`，逾時回 `BULK_SETUP_TIMED_OUT`「Nothing was sent」），避免沒人處理的 Keychain 視窗讓 `bulk_running` 永遠為真。
- **手動解除**：流程是 `begin_release`（Task，讀 Keychain 建 client，只取得 `ReleaseChallenge`）→ 使用者在 `account_panel` 輸入確認句（輸入框一律空白、不預填，只有按鈕可送出，沒有 `on_submit`）→ `ReleaseChallenge::confirm` → `release`（Task：`release_unknown_manually`、移除已存紀錄、寫 `UnknownReleased` 稽核行）。**不帶入原下單參數**：`UnknownRecord` 本來就不含數量／價格／方向，解除後的新進場是新單。成功後 `TradingState::record_released` 清掉 `pairs` 的 `Unresolved`（若另一筆已存紀錄仍指向同 symbol 則保留）。確認句不符時挑戰回到表單、紀錄與封鎖不變。`ChallengeSlot`（見下）是把不可 Clone 的挑戰從 Task 帶回的一次性容器。
- **丟棄**：只對 client id 無效的紀錄（`begin_discard` 對有效 id 拒絕）；只經 `discard_invalid_logged_async`，`logged == false` 顯示「稽核紀錄寫入失敗」；確認句不預填；文案明說帳戶維持封鎖到 app 重啟（exchange 層的封鎖本來就不解除，GUI 只把該筆標成 `Discarded` 並維持封鎖狀態）。掃描測試禁止 `src/` 出現 `discard_invalid(`／`discard_invalid_async(`，`discard_invalid_logged_async(` 與 `cancel_all`／`flatten`（有參數）／`resolve_unknown`／`release_unknown_manually` 只准出現在 `account_flow.rs`（`order_flow.rs` 不准有 bulk／release／discard）。資料層的稽核測試（只記 symbol 與動作，不記 client id）已存在於 `unknown_store`。
- **開機已存紀錄**：`RecordPhase`／`RecordStatus`：Test 通過前顯示「待驗證（Pending verification）」，symbol 保持鎖定；該帳戶本 session Test 通過後 `begin_lookup`（每筆只啟動一次，id 無效者永遠不查）→ `look_up`（`pending_from_record` → `resolve_unknown`，只做讀取）→ `lookup_finished`。找到／推定未下單 → 從鎖定清單移除；仍未知 → 維持鎖定，下一次 Test 會再查。**Test 前不可手動解除**：`RecordStatus::may_release` 不含 `AwaitingTest`（Test 後會先自動查詢，可能發現交易所其實有這張單；先解除會漏掉這個資訊，之後重新進場可能重複曝險）。
- **`acknowledge_trip`** 需要確認視窗（`Kind::AckTrip`，只是本機操作，沒有請求；LIVE 同樣有延遲）。
- **L1**：常駐警示與確認視窗的帳戶名一律取自已存帳戶清單（`display_name`／`name_of`），不用 exchange 層的帳戶 key。**I3**：帳戶編輯器新增帳戶時拒絕重複別名（去頭尾空白、Unicode 小寫比較，`alias_taken`，附測試）；既有存檔若已有重複別名，不改資料，名稱改顯示為 `別名 [venue …id 末四碼]`。刪除中的帳戶（`is_deleting`）不可啟動 bulk。
- **I1**：`order_flow::prepare` 與 bulk／release／lookup 的準備步驟都包在 `within(PREPARE_TIMEOUT = 60 s, …)`；逾時回傳固定文案、丟棄遲到的結果、解凍表單。凍結期間的提示是「Preparing the order… Press Back to cancel.」。測試用 `Duration::ZERO` 搭配永不結束的 future，不用牆鐘或 sleep。
- **新增的一次性容器**：`ChallengeSlot`（`Arc<Mutex<Option<ReleaseChallenge>>>`，只能 `take` 一次，Debug 遮蔽），用意與 `order_flow::IntentSlot` 相同：`ReleaseChallenge` 不是 Clone，而 iced 的 `Message` 必須 Clone。`ReleaseChallenge`、`DiscardToken`、`ManualReleaseToken`、`ConfirmToken` 本身沒有變成 Clone。
- **面板收緊（security-review 後）**：(1) 面板訊息以 client id 定位紀錄（`Release(String)`／`Discard(String)`，由 `record_of` 以 `(帳戶, client id)` 查找），不再用列表索引，列表在 view 與點擊之間被換掉也不會開到別筆；Debug 仍遮蔽。(2) 「刪除中」檢查改看紀錄所屬帳戶（`meta_of_key(record.account)`），不看票據目前顯示的帳戶；`Discard`／`Release` 開表單前與 Submit 時都檢查；票據切換帳戶時 `reset_form()`。`AckTrip` 的確認也檢查 `is_deleting` 與 `bulk_running`（只是本機操作，但不在這兩種狀態下做）。(3) `check_phrase` 在取權杖前重新判斷紀錄狀態（`may_release`／`may_discard`），狀態已變就關閉表單、不產生權杖；該帳戶 `bulk_running` 時，`record_status` 一律回 `Looking`（除了 id 無效者仍為 `Invalid`），因為 bulk 的 `resolve_unknown` 會同時寫入紀錄。(4) bulk 確認的點擊走 `take_confirmed`：權杖取出的同時關閉視窗，第二次點擊找不到視窗、不產生第二個 Task（`Flowsurface` 本身無法在測試中建構，所以測試放在 `bulk_confirm.rs`：`two_confirm_clicks_give_one_token_and_close_the_window`）。(5) gate 拒絕時 `cancel_all`／`flatten` 零寫入請求並回報 `failed`：exchange 層以 `ClosedGate` 測試，`SessionGate` 加 Live 帳戶對 `Reduce`／`Entry` 都回 `LiveTradingNotYetEnabled` 由 `data` 的測試鎖住（`exchange` 不能依賴 `data`，所以分兩層）。
- **全平的保證金模式**：`flatten` 的 `MarginMode::Cross` 只是後備值；每個持倉自帶保證金模式時（`p.margin.unwrap_or(margin)`）以持倉的為準。
- **仍未涵蓋（給後續階段）**：保護單標示、裸倉橫幅（d）；持倉名目價值（需要帶時間戳的標記價）；LIVE 開放前仍需 testnet 實測 hedge 模式全平與撤全部後的重新讀取。

## d1 補充：保護單預設值、`OpenOrder` 擴充、保護狀態純函式（不送單）

- **不新增任何送單呼叫。** d1 的新程式不含 `submit(`、`submit_auto(`、`AutoIntent::new(`；`LIVE_SENDING_ENABLED` 仍為 false。新程式只做資料模型、解析與純函式。
- **預設值資料模型**（`data/src/config/protect.rs`，`AccountMeta.protect`，`serde(default)`）：每帳戶一份，全部預設關閉；舊存檔與 `{}` 都還原成預設（測試 `an_account_saved_before_protect_defaults_loads_with_protection_off`、`an_empty_object_restores_the_defaults`）。內容只有設定，不含機密。止損／止盈以進場**成交價**的百分比或固定價差擇一表示（金額一律十進位字串，不用浮點）。`ProtectDefaults::spec(venue, mode)` 驗證後才交出 `exchange::trade::ProtectSpec`：拒絕非正數與無法解析的數字、止損百分比須嚴格小於 100（100 在 `spec()` 即被拒絕）、止盈段數至多 3、每段 1–100 且總和 100、追蹤回調 0.1–10（Binance 文件值，未實測）；多段止盈只允許 Binance one-way，hedge 與 OKX 一律單段；追蹤止損不允許 hedge 與 OKX。無效設定回傳錯誤，不會悄悄退回「不保護」。
- **自動附掛開關**：`auto_attach`（預設關）與 `auto_attach_live`（預設關）。`attaches_automatically(env)`：LIVE 帳戶兩個都要開。這只是設定；d3 的自動路徑仍須經 exchange 的 `AutoGrant`（d3 才做），並受送單閘門限制。
- **`OpenOrder` 新欄位**（`exchange/src/trade/client.rs`）：`side`、`position_side`（hedge 才有）、`qty`、`trigger_price`、`close_position`、`reduce_only`、`role`（`ProtectRole`：Stop／Target／StopAndTarget／Trailing）。缺欄位一律是 `None`／`false`，缺欄位的訂單**不算**保護（不猜）。Binance：`type`／`orderType` 判斷角色，`closePosition`、`reduceOnly`、`stopPrice`／`triggerPrice`、`origQty`／`quantity`；OKX algo：`ordType` 加 `slTriggerPx`／`tpTriggerPx`（oco 兩者皆有 → StopAndTarget），`move_order_stop` 為追蹤，`closeFraction` 有值視為全平。OKX 普通單與 Binance 現貨單不判角色。測試在 `client.rs` 的 `open_order_tests`。
- **純函式**（`exchange/src/trade/protect.rs`）：`protected_status`、`orphan_protectives`、`plan`。**只有止損算「已保護」**；只有止盈為 `TargetOnly`；止損數量不足或數量未知為 `PartialStop`；`lists_stale` 時全部回 `Undetermined`，文案「Cannot tell: order and position lists may be out of date」，不會顯示成已保護；孤兒偵測在 stale 時回 `None`。保護單判定：必須是 `closePosition`／`reduceOnly`，或 hedge 模式下在該腿的平倉方向；one-way 且非 reduce 的條件單視為進場單，不算保護。hedge 兩腿分開判定，一腿平倉、另一腿仍在時，平掉那腿的保護單算孤兒。`plan`：觸發價取整到 tick 時**朝進場價**（不比設定更寬）、必須嚴格在正確的一側且為正；數量先取整到 step，非最後一段向下取整、最後一段吃剩餘；任何一段拿不到數量就拒絕。
- **`PrivateEvent::Order`（algo 類）不再被丟掉**（`src/trading.rs`、`exchange/src/trade/stream/machine.rs`）：收到 algo 類訂單事件時，(1) GUI 把 `orders_dirty` 設為真，`lists_stale` 回真，直到下一份 snapshot；(2) 串流狀態機對它啟動重新讀取（沿用 `ResyncRequired` 的路徑，受 `MIN_RESYNC_INTERVAL` 間隔限制，讀取中的請求合併成一次重複）。**不做增量更新**：`Triggered`／`Finished` 的語意不等於「已不在清單」。一般（非 algo）訂單事件維持原狀（不觸發重新讀取，避免每筆成交都讀一次）。`HeldPosition` 新增 `dir`，保存 one-way 持倉的方向（線上數量帶正負號，之前只存絕對值）。`TradingState::protection(id)` 給帳戶面板的持倉清單多顯示一行保護狀態文案（沒有新面板、沒有按鈕；橫幅與孤兒處理留給 d5，孤兒函式在 exchange 層已有，GUI 包裝等 d5 有呼叫端再加）。
- **給後續階段**：d2 做票據覆寫與確認視窗文字；d3 做自動路徑與 `AutoGrant`（此時要把 `order_flow.rs`、`order_confirm.rs` 的兩個掃描測試改成白名單）；d4 失敗處理；d5 面板標示、裸倉橫幅、孤兒撤銷確認。未核對：Binance `openAlgoOrders` 與 OKX `orders-algo-pending` 的實際欄位（以文件副本為準，未對 testnet 實測）、`closePosition=true` 在無持倉時的行為（條件 66）。

### d1 安全審查後修正（B1–B3）

- **B1**：OKX `closeFraction` 只有數值等於 1 才算全平；`"0"`、`"0.5"`、空字串、無法解析一律不是（測試 `okx_close_fraction_is_a_full_close_only_when_it_equals_one`）。
- **B2**：追蹤止損不再算止損。新增 `Protection::TrailingOnly`（「Trailing stop only; may not be active yet」），不算已保護，也不計入止損覆蓋的數量；只有追蹤單（含 `closePosition`）時絕不回 `Stop`。
- **B3**：Binance 一般（非 algo）資料列 `role = None`，只有 algo 資料列參與判定（一般單事件不會讓清單過期）。
- **其他**：有 role 但讀不到 `trigger_price` 時 role 設為 `None`（追蹤單例外，它本來就沒有觸發價）；`PartialStop` 文案改為「Stop-loss size unknown or smaller than the position」；`protect` 欄位損毀時退回預設（全關），不會讓整份帳戶清單載入失敗。
- **d5 待辦**：OKX net 模式下同一 `instId` 可能同時有全倉與逐倉持倉，`OpenOrder` 應解析 `tdMode` 並與持倉保證金模式比對。

## d2 補充：進場附掛與確認視窗（仍不新增自動送單）

- **沒有新增送單點。** `submit_auto(`、`AutoIntent::new(` 沒有出現在 `src/`；`.submit(` 仍只在 `src/order_flow.rs`；`LIVE_SENDING_ENABLED` 仍為 false。兩個掃描測試（`order_flow.rs` 的 submit 掃描、`order_confirm.rs` 的權杖來源掃描）沒有新增允許檔案，只更新了說明。新檔 `src/protect_attach.rs` 只做設定解析、文字與記憶體表，不送任何請求。
- **哪些設定適用於一張進場單**（`protect_attach::resolve`）：票據的「本次覆寫」（`PrepareRequest.protect: Option<ProtectDefaults>`，放在請求裡，所以規則過期後重新預覽仍保留）或帳戶儲存的預設值；兩者都只經 `ProtectDefaults::spec(venue, mode)` 驗證，`mode` 用交易所回報的持倉模式（不是存檔值）。合約帳戶持倉模式未知且有東西要附掛時拒絕。hedge 帳戶設多段止盈、OKX 設多段或追蹤，都在準備階段被拒絕並說明原因。
- **覆寫不能打開附掛。** 開關（`auto_attach`，LIVE 還要 `auto_attach_live`）只讀帳戶儲存的預設值，覆寫自帶的旗標一律忽略。開關關閉時確認視窗顯示 "Auto-attach is not enabled …"（LIVE 另加說明有獨立開關），不附掛、不登記。開關在帳戶 modal 的「Protective defaults」編輯區設定（見下），票據不能打開它。
- **準備階段檢查**（`protect_attach::check_against_price`，內部用 `exchange::trade::entry_protection` 即 `plan`）：基準價為限價（限價單）或參考價（市價單，沒有就拒絕）。`Distance` 止損距離不小於基準價時拒絕（不分方向）；任何 `PlanError`（觸發價不在正確一側、份額在 step 下拿不到數量）一律拒絕，不會把設定默默降級。Binance 合約另對各段止盈數量（有追蹤時含總量）套 `check_qty`（市價 lot 規則的最小與最大值，與 `new_algo_order` 相同），不合即在進場前拒絕（d3 前置 (4) 中確認前能做的部分；成交後以實際成交價重算、送出前比對標記價仍留給 d3）。
- **OKX 進場附掛**：`ManualContext.protect: Option<&ProtectSpec>`；`build_intent` 對限價與市價進場組單段 `OkxAttach`（觸發價由 `plan` 取整，附掛 `ClientId` 在 builder 內產生，沿用既有 `Role`），預覽 `fields` 含 `attachAlgoOrds[0].*`。空 spec 不附掛；追蹤或多段在 builder 層也拒絕（`TradeError::ProtectionNotPossible`）。`prepare_steps` 在列出附掛前核對預覽確實含 `attachAlgoOrds[0].*`，沒有就拒絕。限價離市價很遠時，OKX 的 `leg()` 會以參考價（市價）檢查觸發方向，可能回 `TriggerOnWrongSide`，這會讓進場被拒而不是無保護送出。Binance 合約進場單內容不變（測試 `a_binance_entry_is_sent_unchanged_whatever_the_protection`）。測試：`the_preview_of_an_okx_entry_with_attached_protection_equals_the_json_the_server_receives`（預覽等於伺服器收到的 JSON，附掛含在內）、`an_okx_entry_carries_the_attached_triggers_in_its_preview`、`okx_refuses_what_an_entry_cannot_carry_and_never_sends_it_bare`。
- **`ProtectSpec` 納入 `trailing` 與 `trigger_price`**（d1 審查 d3 前置 (1)）：`spec()` 驗證後保留兩者；只設追蹤止損時 spec 不為空（`ProtectSpec::is_empty`）；`is_empty` 與 `ProtectDefaults::is_empty` 不一致時回 `ProtectError::Dropped`，不會把有內容的設定當成「沒東西」。測試 `the_spec_keeps_trailing_and_the_trigger_price`、`both_is_empty_answers_agree`。`plan` 的輸出不含追蹤。
- **確認視窗**（`protect_attach::confirmation_lines`）：OKX 列出附掛的 TP／SL（讀自預覽欄位），並註明只在進場**完全成交**後才由交易所建立、app 關閉仍有效；Binance 合約列出「進場成交後自動送出」區塊（止損／各段止盈／追蹤、互撤、**只在 app 執行中有效，離線或斷線不保護**，條件 81-2）；沒有設定保護時明說 "No protective orders are set"。確認一次涵蓋這些內容，LIVE 的 700 ms 延遲不變。**（d3 已接上送出路徑）**：`AfterFill` 區塊開頭為 "Sent automatically after the entry fills:"，B2 的過渡警告與測試 `the_after_fill_block_says_it_is_not_sent_by_this_build` 已移除，改為 `the_after_fill_block_says_it_is_sent_automatically`。
- **`PendingAttach` 記憶體表**（`PendingAttachTable`，放在 `TradingState`，不存檔、不寫 log，`Debug` 不印 id）：key 為進場單 client id，值為確認當時列出的保護內容（帳戶、symbol、方向、是否限價、種類 `OkxOnEntry`／`AfterFill`、spec、列出的文字）。確認送出時由 `main.confirm_order` 登記（內容取自確認視窗保存的 `AttachPlan`，不從帳戶目前設定重算）；`SendSummary` 不帶完整 id，所以用 (帳戶, symbol) 對回 key。結果處理（`retention`，不用萬用分支）：`Accepted`／`Found`／`Unresolved`／`Partial` 保留；`Rejected`／`NotSent`／`Refused`／`RulesExpired`／`ConfirmationMismatch`／`NotPlacedPresumed` 移除（後三者沒有送出，brief 只列前三者，這是補充判斷）。條目在進場成交或被撤後**不會自動消失**（d3 才消費），所以面板文字與關閉提醒會持續到重啟或帳戶被刪除；進場結果為 `Unresolved` 後，由 `account_flow` 查到未成立（`NotPlaced`）、或被手動解除／丟棄時，以紀錄的 client id 呼叫 `TradingState::entry_gone` 移除。
- **面板**：`TradingState::protection` 對有 `PendingAttach` 的 symbol 一律回新的 `Protection::EntryPending`（"Entry protection pending, not in force yet"），即使清單上有止損、或清單過期也是；不會顯示成已保護。`Protection::EntryPending` 只由 GUI 設定，`protected_status` 不產生。
- **關閉提醒**（條件 81-3）：主視窗收到關閉要求，且有「Binance 合約、由 app 成交後送出（`AfterFill`）」的限價進場 `PendingAttach` 時（OKX 的附掛在交易所端，app 關閉仍有效，不算），第一次改為 toast 警告並不關閉；10 秒內第二次關閉要求才關閉（`protect_attach::hold_close`，純函式）。
- **條件 75(4)**：測試 `the_form_takes_no_value_from_an_indicator_or_the_chart` 掃描下單表單相關檔案不含指標／圖表字樣，且 `main.rs` 對票據只送 `Prepared`，其他檔案不產生會寫入表單欄位的訊息。
- **仍未涵蓋（d3 之後）**：消費 `PendingAttach` 的自動路徑與 `AutoGrant`、成交後以實際成交價重新計算、互撤、失敗處理（條件 65）、孤兒偵測。OKX 附掛的 `tdMode`、`attachAlgoOrds` 在 `orders-pending` 的回傳尚未解析；以上與 d1 列出的未核對欄位都需要 testnet 實測。
- **帳戶 modal 的保護單預設值編輯器**（`src/modal/exchange_accounts.rs` 的 `ProtectForm`，仿 `RiskForm`）：止損（關／百分比／固定價差）、止盈（關／最多 3 段，每段距離與整數百分比）、追蹤（回撤比例、可選啟動價）、觸發價類型、`auto_attach`；LIVE 帳戶另有獨立的 LIVE 自動附掛開關（預設關；LIVE 交易尚未開放時仍可儲存），非 LIVE 帳戶忽略該開關。數值以字串編輯、儲存時解析，輸入框不預填非數值，也不寫 log。儲存一律經 `ProtectDefaults::spec`（與下單準備同一個驗證），無效（負數、止損百分比 ≥ 100、段數 > 3、比例總和 ≠ 100）時顯示錯誤、不寫入。hedge 帳戶設多段在準備階段才被拒絕。票據的「本次覆寫」仍只能收窄、不能打開附掛。

### d2 安全審查後修正（B1、B2、S1–S5）

- **B1（反向單不當成進場附掛）**：`PrepareInput.held: Holdings`（`protect_attach`，由 `main.rs` 從 `TradingState::positions` 與 `lists_stale` 取該 symbol 的持倉方向）。`prepare_steps` 在 `resolve` 之後、組附掛之前，只要結果是 `Resolved::Attach`，就呼叫 `protect_attach::check_new_entry`：one-way 合約帳戶有反向持倉，回 "This order reduces or reverses a position; protective defaults apply to new entries only. Choose "No protection" to send it."；持倉清單過期或未知，回同類訊息並說明無法判定。`Resolved::Nothing`／`SwitchOff` 不受影響，所以選 "No protection" 仍可送出減倉單。hedge 帳戶與現貨不受影響。縱深防禦：`entry_of(spec, product)` 對現貨 `Short` 回 `None`，不產生 `AttachNote`。測試：`one_way_contract_orders_against_a_position_or_unknown_positions_are_refused`、`hedge_and_spot_accounts_are_not_affected_by_the_position_check`、`choosing_no_protection_leaves_nothing_to_check`、`a_spot_sale_is_never_an_entry_so_it_gets_no_protection_note`。
- **S1**：`PendingAttachTable::register` 已有同 (帳戶, symbol) 的 sending 時拒絕登記（回 `false`、計數、`TradingState::register_attach` 記 warn），不再覆蓋刪舊條目。測試 `a_second_registration_for_a_send_in_flight_is_refused`。
- **S2**：關閉提醒只攔 `window::Event::CloseRequested`（主視窗）。macOS 的 Cmd+Q／選單結束是否經過這裡，無法在不執行 app 的情況下確認（winit 的 `terminate:` 可能不送 `CloseRequested`）；**列為手動確認項**：有 Binance `AfterFill` 限價進場單 pending 時按 Cmd+Q，看是否出現關閉提醒。
- **S3（d3 已做，見 d3 補充）**：prepare 與 confirm 之間帳戶預設被改掉（例如關掉 `auto_attach`）時，d3 消費 `PendingAttach` 前要再檢查一次開關，以較嚴者為準（開關已關就不送，並提示）。
- **S4**：`ExchangeAccounts` 的 `RiskSave`／`ProtectSave` 對正在刪除的帳戶不處理。測試 `an_account_being_deleted_takes_no_risk_or_protect_save`。
- **S5**：Distance 文案帶 quote 幣別（`protect_attach::quote_of` 由 symbol 名稱判讀，取不到就寫 "price units"）。測試 `a_distance_names_its_quote_currency_or_price_units`。

## d3 補充：成交後自動附掛、互撤、`AutoGrant`（第一個呼叫 `submit_auto` 的段）

這一段第一次讓 app 自己送單（Binance USDⓈ-M 的保護單與撤單）。送單點仍只有兩個：手動單 `src/order_flow.rs`（`.submit(`），自動單 `src/auto_flow.rs`（`submit_auto(`、`AutoIntent::new(`，**唯一允許檔案**）。`LIVE_SENDING_ENABLED` 仍為 false；Live 帳戶的自動單和手動單一樣被 `SessionGate` 擋下（測試 `a_closed_gate_stops_a_granted_request_too`；`trade_gate` 既有的 Live 測試涵蓋 Reduce 與 Entry 兩類）。

### 1. `AutoGrant`（`exchange/src/trade/grant.rs`）
- **由確認過的 `OrderIntent` 產生**（`AutoGrant::from_entry`，在 `confirm_order` 內、進場送出之前）：帳戶、symbol、方向、數量都取自 intent，不收呼叫端的字串。只接受 Binance USDⓈ-M `POST /fapi/v1/order` 且 `Exposure::Adds` 的進場；角色清單（止損、最多 3 段止盈、追蹤）來自確認視窗列出的 `ProtectSpec`。產生失敗就不送進場（顯示錯誤）。
- **不可 `Clone`**（`compile_fail` doctest），`Debug` 遮蔽；App 端的 `GrantCell` 只是包 `Arc<Mutex<_>>` 的把手，鎖只在一次檢查內持有，不跨 await。
- **`submit_auto` 現在收 `GrantedAuto`**，只有 `AutoGrant::authorise` 能造出（欄位 `pub(crate)`）。檢查：帳戶、symbol、path／method 配對、`clientAlgoId` 是這個 grant 發出的、只減倉、未過期（只限新掛單）、角色對應 `type`、平倉方向、`positionSide`、已有成交、`closePosition` 或 `reduceOnly` 且數量 ≤ 累計成交；id 單次使用；撤單只能撤「這個 grant 發出且已送出、尚未撤過」的 id。檢查通過才改狀態。
- **撤單權限比掛單期限長**：grant 過期後不能再掛，但仍可撤自己掛過的單。
- `RiskClose`（風控平倉）走另一條 `submit_auto_unbound`（`pub(super)`，只有 `bulk.rs` 呼叫；`source_rules_tests.rs` 的 `a_granted_request_is_made_by_the_grant_only_and_the_unbound_send_is_for_bulk_only` 掃描）。
- 測試：`grant/tests.rs`（帳戶不符、symbol 不符、Adds、數量超過、角色不在清單、過期、重用、撤單重複皆被拒；相符才送）、`send_tests.rs::auto_grant`（假伺服器：只有 grant 涵蓋的請求到達伺服器）。

### 2. 部分成交策略（`exchange/src/trade/auto_attach.rs`、`src/auto_flow.rs`）
Binance 條件單不能修改，所以**不做「撤舊掛新」**：
- **首筆成交**：送一張 `closePosition` 的 `STOP_MARKET` 全平止損（涵蓋之後所有成交的部位）。
- **進場結束**（`Filled`，或部分成交後 `Canceled`／`Expired`）：依累計成交量與均價，one-way 送止盈（`reduceOnly`，數量依份額，最後一段吃剩餘）與追蹤；hedge 只送單段 `closePosition` 止盈、不送追蹤。
- `Filled` 更新帶的數量比已收到的成交多時，等成交補齊再送（不猜）；進場結果 Unknown 時只累計、不送，查到 Found 才送。
- 部分成交後取消，只保護已成交的量；沒成交就取消的進場直接移除。
- 價格以實際均價重算，逐段 `check_qty`，並以當下標記價（取不到就用成交均價；報告有說明）檢查不會「一送出就觸發」。任一段算不出來，整個階段都不送（不半送）。

### 3. 互撤（`CancelSibling`）
- **（以第 8 節為準，已改）** 規則以「被撤的對象」來定：止損觸發 → 撤止盈與追蹤；追蹤觸發 → 撤止盈；全平止損只在「有保護單 `Finished`、清單新鮮、該方向已無部位」時才撤。事件重複不重撤（狀態已離開 `Open`）。每次撤單都經 grant 的 `cancel_intent` → `authorise`。撤單被拒絕不再當成「已不存在」（第 8 節 M1）。

### 4. 不重試、可見的失敗
- `PlanError`、`check_qty`、Rejected、Unknown、429、grant 拒絕一律**不重送**，只寫成帳戶面板的一行錯誤（`TradingState::auto_notes`，最多 12 行，英文，含交易所 `code`，不含 client id 與交易所 `msg`），供 d4 做按鈕。Unknown 只做唯讀查詢（`resolve_unknown`）。
- 面板開關（S3）在**送出當下**再讀一次（`run_auto_jobs` 的 `switch`），關掉就不送並顯示「not protected」。grant 過期的失敗也有一行。

### 5. 其他 d3 項目
- **N1**：`confirm_order` 在取出 intent 前以當下 `Holdings` 重跑 `check_new_entry`，與預覽時不同（現在反向或清單過期）就不送、要求重新預覽。**N3**：`TradingState::holdings(id, symbol)`，prepare 與 confirm 共用（測試 `holdings_name_the_directions_on_the_symbol_and_say_when_the_list_is_stale`）。
- **resync 活性**（`stream/machine.rs`）：algo 事件觸發的讀取最小間隔 8 秒（`ALGO_RESYNC_INTERVAL`）；連續 3 次（`MAX_RESYNC_REPEATS`）因事件被折疊而重讀後，不再追，snapshot 照送、清單仍標為過期並 `log::warn!`，32 秒後再讀一次。測試 `an_algo_event_waits_longer_than_other_reasons_for_a_read`、`events_that_keep_arriving_cannot_keep_the_reads_going_for_ever`。
- **偏離 4**：OKX 限價進場的附掛 TP／SL 方向改以限價（進位後送出的價）檢查，不再用市價（`okx.rs::place_order`，測試 `a_limit_entry_checks_its_attached_triggers_against_the_limit_price`）。
- **OKX `attachAlgoOrds`**：`orders-pending` 的一般單解析出 `OpenOrder.attached`（只標種類，**不是**保護中，`role` 仍為 `None`）；帳戶面板對掛著限價進場、尚無持倉的 symbol 顯示 "Protection is attached on the exchange; it acts only once the entry is fully filled"。測試 `an_okx_entry_with_attached_orders_says_which_kind_without_being_in_force`。
- **移除 d2 B2 過渡文字**（見 d2 補充）。`protect_attach.rs` 模組說明不再寫「沒有東西消費」。

### 6. 掃描測試（明列允許檔案）
- `order_flow.rs::submit_is_called_from_this_file_only_and_the_automatic_paths_from_auto_flow_only`：`.submit(` 只在 `order_flow.rs`；`submit_auto(`／`AutoIntent::new(` 只在 `auto_flow.rs`；全 `src/` 不出現 `block_on(`。
- `order_confirm.rs::the_tokens_come_from_the_stored_values_in_the_confirmation_files_only`：`for_preview(`／`for_bulk(` 規則不變；`auto_flow.rs` 只允許唯讀的 `.resolve_unknown(`。
- `trading.rs::the_only_trade_calls_here_are_reads`：`trading.rs` 仍不含任何送單呼叫。注意：掃描以第一個 `#[cfg(test)]` 截斷，所以 `auto_flow.rs` 的正式程式碼不能在測試模組之前放 `#[cfg(test)]` 項目。

### 7. 給 security-review 的重點與已知限制
- 重點 diff：`exchange/src/trade/grant.rs`（`authorise`、`issue`、`cancel_intent`）、`client.rs`（`submit_auto`／`submit_auto_unbound`）、`src/auto_flow.rs`（`run`、`Sender::step`、`AutoBook`）、`src/main.rs`（`confirm_order`、`run_auto_jobs`）。
- 測試用的 `test-support` feature（`exchange/Cargo.toml`）只由根 crate 的 dev-dependency 啟用，內容是固定的 BTCUSDT 規則；正常 build 不含。
- `auto_flow::run` 本身（Keychain → client → gate）沒有假伺服器測試（GUI 的 tokio 只有 `rt` feature）；送出行為由 exchange 的 `send_tests::auto_grant` 與 `auto_attach` 測試涵蓋，`run` 與 `order_flow::send` 結構對應，需手動 testnet 確認。
- 風控日誌寫入失敗不阻擋自動送單（`Sender::step` 只 `log::warn!`），與手動路徑一致（d3 審查順手 3）。
- 已知缺口：app 在成交前關閉就不會附掛（確認視窗與關閉提醒已說明）；Hedge 帳戶沒有追蹤、止盈單段；沒有現價時以成交均價檢查觸發方向。

## 8. d3 審查後修正（M1–M4、grant 綁定、順手項）

security-review 判 BLOCK 的四項與 AutoGrant 綁定補齊（2026-11-11 使用者決定不延後）。所有失敗都寫成帳戶面板的一行（`TradingState::auto_notes`），沒有「靜默失去保護」的路徑。

### M1 撤兄弟單的拒絕與 Unknown 可見
- `auto_flow::rejected_step`：撤單只有「交易所拒絕、`RejectKind::Exchange`、且 code 在 `CANCEL_NOT_FOUND`」才算 `Gone`。**白名單目前留空**（algo 撤單「不存在」的 code 尚未對 testnet 核對，見 `unknown.rs`），所以 429／418／403／時鐘／任何交易所錯誤一律 `Failed`，文案為 "could not be cancelled (the exchange refused it, code X); it may already be gone — check the open orders and cancel it by hand"。
- 撤單結果 `Unknown`（DELETE 沒有 lookup）同樣寫 note，不重試。`Failed`／`Unknown` 的單改為 `Live::CancelFailed`（不是 `Cancelled`），條目留在 book；之後 algo `Canceled` 事件會把它轉成 `Cancelled`。
- 測試：`a_refused_cancel_is_a_failure_unless_the_code_says_the_order_is_gone`、`a_cancel_that_failed_or_is_unknown_is_written_down_and_not_taken_as_cancelled`。

### M2 不再靠「觸發」撤掉唯一的全平止損
- `Live::Fired` 拆成 `Triggered` 與 `Finished`，`Triggered → Finished` 可升級。止損觸發 → 撤止盈與追蹤；追蹤觸發 → 撤止盈；止盈觸發什麼都不撤。
- 全平止損只由 `release_stop` 釋放：有（非止損的）保護單 `Finished`、清單新鮮（`!stale`）、且 `Holdings` 已無 `e.dir` 方向的部位，才把仍開著的單全部撤掉。清單 stale 就等（不寫 note、不撤）；部位仍在就保留止損，並在「已沒有其他開著的減倉單」時寫一次 note "The other protective orders filled but a position remains; the stop-loss was kept."（每個條目一次）。`fired()` 與 `done()` 的掃描都跳過 `StopLoss`。
- 觸發後 `Rejected`／`Expired` 視為未生效：該單轉 `Failed`、止損的 `closed` 還原，並寫 note。
- 既有測試 `a_take_profit_cancels_the_stop_only_when_it_was_the_last_one` 的規格改變（止盈 `Triggered` 不再撤止損），改為下列測試：`a_triggered_take_profit_does_not_cancel_the_stop`、`a_finished_take_profit_with_a_position_left_keeps_the_stop_and_says_so_once`、`a_finished_take_profit_with_a_fresh_flat_list_cancels_the_stop_once`、`a_finished_trailing_stop_with_a_position_left_never_cancels_the_stop`、`a_stale_list_holds_the_stop_until_it_is_fresh`、`an_order_refused_after_it_triggered_did_not_take_effect_and_says_so`。
- 偏離：「部位仍在」的 note 只在沒有其他開著的減倉單時才寫（多段止盈的第一段完成、部位本來就該還在，不算異常）。

### M3 同帳戶同 symbol 一次一筆自動附掛
- `AutoBook::is_active` 與 `TradingState::auto_active(id, symbol)`；`prepare`（`PrepareInput.auto_active`，只對 Binance 合約的附掛）與 `confirm_order`（N1 檢查旁）都檢查，文案 `protect_attach::AUTO_STILL_ACTIVE`。只看還有工作要做的條目：送出中、有階段未結束、或有開著的單且最後結果不是 Unknown／`CancelFailed`；進場結果 Unresolved（另有 Unknown 鎖）與已放棄（`attention`）的條目不算，所以不會永久擋住 symbol。
- 測試：`an_entry_with_work_left_is_active_and_one_that_was_given_up_is_not`、`an_unknown_entry_or_order_does_not_block_the_symbol_for_ever`、`a_second_entry_with_automatic_protection_waits_for_the_first_to_end`（trading）。d4 再提供「我已處理」按鈕。

### M4 沒有通知就沒有保護的路徑
- **M4-1**：結束狀態（`Canceled`／`Expired`／`Rejected`）一律用 `update.filled_qty` 當 `expected`；交易所回報的量大於已見成交時不悄悄移除，寫 "Some fills of the entry were not seen; the position may not be protected. Check it."。已見成交為 0 時放棄該條目（不再送）；已見成交 > 0 時仍依已見的量掛止盈。測試 `an_entry_that_ended_with_fills_not_seen_is_not_removed_quietly`、`a_status_with_more_filled_than_seen_still_protects_what_was_seen`。
- **M4-2**：去重鍵改為 `(symbol, trade_id)`（`AccountRuntime::note_fill(symbol, trade_id)`）。測試 `the_same_trade_id_on_two_symbols_is_two_fills`。
- **M4-3**：`e.closed`（止損已觸發）後不掛止盈與追蹤（`rest` 設為 `NotWanted`），之後的成交寫一次 note "The entry filled again after its stop-loss closed the position; that part is not protected."。測試 `a_stop_that_closed_the_position_gets_no_take_profit_and_a_later_fill_is_noted`。
- **M4-4**：重連後第一份 snapshot（`book_trusted` 由 false 轉 true）時，`AutoBook::after_reconnect` 對尚未完成的條目檢查：沒有可能是它的進場掛單、且該 symbol 有 `e.dir` 方向的部位（或已見成交少於預期），就寫 note 並放棄該條目，**不自動補送**（補送屬 d4 的手動重試）。偏離：`OpenOrder.id` 是交易所單號、不是 client id，所以「進場還掛著」以 symbol、開倉方向、非 reduce-only 的一般單判斷。測試 `a_reconnect_that_finds_a_position_and_no_entry_order_notes_it_and_sends_nothing`、`a_reconnect_does_not_cry_wolf`、trading 的 `a_reconnect_that_finds_a_position_and_no_entry_order_is_noted_and_sends_nothing`。

### AutoGrant 綁定（`exchange/src/trade/grant.rs`）
- grant 存**進場 client id** 與 **spec 指紋**（`ProtectSpec::fingerprint()`，對停損／各段止盈（觸發價基準、百分比或價差、份額）／追蹤參數做雜湊，只在記憶體，不存檔）。`from_entry(intent, roles, &spec, ..)` 從 intent 取 client id。
- 掛單必須帶 `Basis`（`AutoIntent::with_basis`，由 `StageInput::basis(entry_id)` 以同一個 `spec` 產生）；`check_place` 比對：沒有 basis → `NotBound`、client id 不符 → `WrongEntry`、指紋不符 → `SpecMismatch`。撤單不需要 basis（id 一定是 grant 發出的）。
- 每個角色累計已授權的 `reduceOnly` 數量 ≤ 累計成交（`TotalTooLarge`），止盈各段加總不會超過 filled；授權即計入，失敗的送出不退回（不重送）。
- `add_fill(entry_id, qty)` 只接受與 grant 進場 client id 相符的成交（回傳 `false` 且不加），`AutoBook::on_fill` 也不計入本地的 `filled`，兩邊保持一致。
- 測試：`grant/tests.rs::a_placement_must_name_the_grants_entry_and_the_confirmed_settings`（無 basis、別的進場、停損距離、份額、觸發價基準）、`a_fill_of_another_entry_is_not_added`、`the_take_profit_legs_together_stay_within_what_filled`；假伺服器測試改為帶 basis。
- 限制：進場 client id 與 spec 仍由呼叫端（`auto_flow.rs`，唯一允許檔案）傳入 basis，grant 只比對、不能自行推得；這屬於「同一個檔案內的一致性」，由掃描測試限制只有 `auto_flow.rs` 能用 `.with_basis(`、`.authorise(`、`.cancel_intent(`、`AutoGrant::from_entry(`、`.add_fill(`，且 `auto_flow::mint(` 只在 `main.rs`（相對路徑比對，不只看檔名）。

### 順手項
- 順手 2：`register_attach`／`register_okx_attach` 回傳 `bool`（`#[must_use]`），`confirm_order` 收到 false 就不送進場（fail closed）。
- 順手 5：確認視窗加一行 "Protective orders are placed only within 24 hours of this confirmation."。
- 順手 3：風控日誌寫入失敗不阻擋自動送單（見第 7 節）。順手 4（撤單 Unknown 文案放到按鈕旁）留給 d4。

### 仍待 testnet 與後續
- Binance algo 撤單「不存在」的 code（填進 `CANCEL_NOT_FOUND`）；同方向第二張 `closePosition` 是否被拒；`TRIGGERING`／`TRIGGERED`／`FINISHED` 的語意與觸發後市價單被拒的狀態序列；`closePosition` 在無部位時的行為。
- 互撤狀態只在記憶體；Live 開放前要補啟動時對帳殘留自動單。

## d3 複核修正（M5 與順手項）

- **M5**：`AutoBook::after_reconnect` 對「沒有掛著、沒有該方向部位、也不短少」的條目不再靜默略過，也不 `drop_entry`（`resting == false` 不一定可信），改為 `give_up` 並寫 note（`NOT_FOUND_OPEN`）。已掛出的保護單留在 book、事件照常更新；條目不再算作用中，不會擋住 symbol。測試 `a_reconnect_that_finds_no_entry_and_no_position_stops_and_notes_once`、`a_reconnect_with_a_fill_seen_keeps_the_stop_and_sends_no_take_profit`。
- **順手 1**：`done()` 只在該單為 `Live::Cancelling` 時才寫 `CancelFailed` 與 note。測試 `a_cancel_result_that_comes_after_the_event_does_not_report_a_failure`。
- **順手 3**：止損 `Triggered` 後收到 `Canceled`，比照 Rejected／Expired（`Failed`、`closed = false`、寫 note）。測試 `a_stop_that_triggered_and_was_cancelled_is_failed_and_noted`。此序列仍待 testnet 確認是否真的會發生。
- **N1（resync 被當成重連）**：複核發現 `after_reconnect` 在每次 resync 都跑（包含只由 algo 事件引起的），而且在被扣住的事件套用前就判斷，可能讓正常成交沒有止損。修法：
  1. `AccountRuntime.recheck_auto` 只在 `Reconnecting`／`AuthFailed`／`Unavailable`、重建 stream、`PrivateEvent::ResyncRequired` 時設；純 algo 事件的 resync 不設。
  2. 檢查改在 `State(Connected)` 且 `book_trusted && take(recheck_auto)` 時才做（driver 的順序是 snapshot、被扣住的事件、`State(Connected)`），不在 `Snapshot` 分支。resync 失敗或在 Connected 前又斷線，旗標保留到下一次。
  3. `after_reconnect` 改用 `give_up_rest`：只把 `rest` 由 `Todo` 改 `Failed`、設 `attention`、寫 note；`stop` 保持 `Todo`，所以之後看到**該單自己 client id 的成交**時仍會送出止損（closePosition，連沒看到的成交也一起覆蓋；`grant.add_fill` 擋其他單，方向錯誤由 `held` 檢查擋）。沒有成交就不送任何東西，不是補送。因 `stop` 未終結，`after_reconnect` 在 `!reported` 時設 `reported = true` 並 push 到 `finished`，讓 attach 表與「等待保護」提示能清掉。`NOT_FOUND_OPEN`／`LOST_CONNECTION` 文案改為「take-profit 與 trailing 已停止；看到成交仍會掛止損；請檢查部位」，不再說 "not protected"（一併處理 R1）。
  測試：trading 的 `a_resync_caused_by_an_algo_event_is_not_a_reconnect`、`a_reconnect_notes_and_a_later_fill_still_gets_its_stop`、`a_resync_required_event_is_treated_like_a_reconnect`，以及調整後的 `a_reconnect_*`（含 auto_flow 兩個：之後的成交仍產生 Stop 工作、不產生 Rest 工作、`take_finished` 含該條目）。
  - 仍待 testnet：resync 期間成交時 snapshot、被扣住事件與 `State(Connected)` 的實際到達順序；Binance 是否以 -4130 拒絕重複的 closePosition 止損。

### d4 待辦（本次不實作）
- **`Filled` 但有未見成交**：記錄 `expected` 設定時間，poll 時超過 N 秒仍短少就寫 `FILLS_NOT_SEEN`，`filled == 0` 時 `give_up`；有 client id 但沒有 trade id 的成交至少 `log::info!`。
- **永久擋住 symbol 的其他情況**（例如止損一直 `Open` 但部位已被手動平掉）：d4 的「我已處理」按鈕要涵蓋所有 `is_active` 的條目，不只 Unknown／CancelFailed。

## M5 d4：保護失敗處理（條件 65）

### 1. 保護失敗狀態（`src/auto_flow.rs`）
- `FailReason` 共 18 種：`Rejected`、`RateLimited`（429／418）、`NotSent`、`Unknown`、`PlanError`、`QtyCheck`、`GrantRefused`、`SetupFailed`、`StaleReference`、`SwitchedOff`、`ReducedInstead`、`NotTakingEffect`、`CancelFailed`、`CancelUnknown`、`FillsNotSeen`、`FilledAfterClose`、`NotFoundOpen`、`LostConnection`。每種有固定英文文案（`FailReason::text`，測試 `every_fail_reason_has_a_text`）；`may_leave_bare` 表示「部位可能沒有止損」。
- 失敗存在 `AutoEntry.failures`（只在記憶體，不進 `saved-state.json`）。有失敗的條目不會被 housekeeping 移除，直到使用者按「I've handled it」。
- `is_active` 語意不變：失敗或 Unknown 的條目不擋 symbol（Unknown 另有 c2b-3 的紀錄鎖）。
- 所有文案不含 client id、價格或交易所回傳原文；`FailureView` 的 `Debug` 也不印 client id。

### 2. 帳戶面板（`src/modal/account_panel.rs`）
- 常駐紅色橫幅：每個失敗條目一則（alias、symbol、各原因文案），按鈕：
  - **Retry protection**：只在 `may_retry`（部位可能沒有止損、且沒有 app 的止損還立著、條目已送出）時出現。
  - **Flatten (close all positions)**：重用 `Action::Bulk(BulkAction::Flatten)`，同一個確認視窗與 bulk 權杖，不另開路徑。
  - **I've handled it**：兩段式（第一下出現說明與 Yes／Back，第二下才執行）。涵蓋所有仍算作用中的條目（沒有失敗但還擋著 symbol 的也會列出，只有這個按鈕）。
- 撤單 Unknown／撤單失敗（M1）的文案直接寫在該條目的橫幅裡、按鈕旁。
- 不是失敗的提示（例如 `STOP_KEPT`）仍用黃色提示（`auto_info_notes`）。

### 3. 重試走既有手動路徑，不走 `submit_auto`
- 理由：`AutoGrant::from_entry` 需要進場的 intent，重試沒有；為了重試硬造 grant 會讓「舊 grant 可被重用」變成可能。
- 流程：`protect_attach::retry_stop` → `ManualSpec::Stop { size: ClosePosition }` → `start_review` → 確認視窗 → `order_flow::send`。因此有**新的 client id、新的 confirm token、沒有 grant**，必須使用者在確認視窗按下確認才會送出；沒有任何自動送出。
- `retry_stop` 拒絕：清單過期、帳戶沒有該方向部位、持倉模式未知、預設設定不合法或沒有止損、沒有報價或報價過舊。
- 手動止損被接受（`Accepted`）後，`main.rs::on_order_sent_inner` 呼叫 `TradingState::protection_added`：清掉該條目「部位可能沒有止損」的失敗，並把尚未送出的止損階段改成 `Failed`，所以舊 grant 之後不會再送第二張止損。被拒或 Unknown 的重試不清除失敗；`SendResult::Found`（Unknown 之後在交易所找到該單）也不清除，橫幅與 Retry 按鈕仍在，使用者確認部位後可按「I've handled it」。
- `protection_added` 作用於同一帳戶、同一 symbol、同一方向的**所有**條目（包括進場單還在掛單的條目，以及使用者自己從訂單票下的 closePosition 止損）。closePosition 止損涵蓋整個持倉，所以這樣是安全的；但若使用者之後手動撤掉那張止損，這些條目不會自動補止損。
- 重試止損的確認視窗寫明觸發基準是 mark price（帳戶的保護預設可能用 last price）；`check_fills_wait` 的等待改用單調時鐘（`Instant`，由 `PollEnv::mono_ms` 注入），系統時間被調整不會縮短或拉長等待。通知以帳戶 id 去重，alias 會濾掉 Unicode Cf 字元。
- 偏離：只重試止損（止盈走訂單票）；基準價是當下報價（`HeldPosition` 沒有進場價）；手動止損一律監看 MARK_PRICE（`manual.rs` 寫死），不是帳戶設定的觸發基準。預覽視窗會列出這些數字。
- 掃描測試：`submit_auto(`、`AutoIntent::new(` 仍只在 `src/auto_flow.rs`，`.submit(` 仍只在 `src/order_flow.rs`，`for_preview(` 仍只在 `order_confirm.rs`。新增掃描測試 `the_stop_and_cancel_offers_only_open_the_review_and_send_nothing_themselves`（`start_protection_retry` 的函式本體含 `start_review(`，且不含 `order_flow::send(`、`confirm_order`、`.submit(`、`submit_auto(`、`AutoIntent::new(`、`auto_flow::mint(`、`AutoGrant`）。本段動到的掃描測試：`order_flow.rs` 的 submit／auto／mint 掃描（切割條件改變，兼容 `mod tests` 與 `pub(crate) mod tests` 兩種寫法）、`trading.rs` 的 `the_only_trade_calls_here_are_reads`（同樣改切割條件，否則新的單項 `#[cfg(test)]` 會讓它只掃到檔案前段而空過）。`order_confirm.rs`、`order_ticket.rs` 的掃描沒有單項 `#[cfg(test)]` 在測試模組前，不受影響。`order_flow.rs` 的掃描測試改為只排除 `#[cfg(test)]\nmod tests` 之後的內容（原本遇到第一個 `#[cfg(test)]` 就截斷，會漏掉 `auto_flow.rs` 在測試模組之前的 `#[cfg(test)]` 單項）。允許檔案清單不變。

### 4. 「I've handled it」語意
- 只從 `AutoBook` 移除條目並解除該條目對 symbol 的阻擋；`Sent::Sending` 或 `busy`（有自動單正在送）時拒絕（`DismissError::Busy`）。
- 不碰 `pairs`、`UnknownStore` 或 c2b-3 的紀錄；Unknown 紀錄的釋放／丟棄仍須手動流程的確認句，沒有新增繞過。
- 風控日誌多一種 `LogAction::ProtectionHandled`（理由 `protection_handled_by_user`，只有帳戶與 symbol）；寫入失敗只在面板提示，不阻擋移除。

### 5. 參考價新鮮度
- 資料來源：側欄 ticker 表的價格（Binance 實際上是 REST 的最新成交價），表顯示時每 13 秒更新、隱藏時每 300 秒更新。側欄不在授權範圍，所以 `main.rs` 在 `UpdateStats(venue, _)` 成功時按交易所記錄時間（`quote_stamps`）。
- `REF_PRICE_MAX_AGE_SECS = 330`（隱藏時的 300 秒加 30 秒餘裕；更短會在側欄隱藏時讓所有自動止損停擺）。超過就視為未知：該階段不送、歸為 `StaleReference`，交給重試。
- `auto_flow::reference_price(basis, quote)`：觸發基準為 last 且報價夠新 → 用報價；基準為 mark → 沒有 mark 來源，不使用報價，沿用成交價（d3 已接受的偏離）；完全沒有報價 → 沿用成交價；報價過舊 → `StaleReference`。
- **重要限制**：只有觸發基準為 last 的規格會檢查報價是否過舊；基準為 mark 的規格（測試用 `spec()` 預設就是 Mark）沒有 mark 來源，過舊檢查**不會觸發**，一律沿用成交價。另外 `UpdateStats(venue, _)` 只在該交易所的 ticker 抓取成功時發出（失敗走 `StatsFetchFailed`，不蓋時間），但時間戳是整個交易所一份：同一交易所的現貨與永續在同一次抓取內更新，若只有部分市場有更新，時間戳仍會前進。
- Live 開放前建議改成每個 symbol 的即時價格串流，並區分 mark／last。

### 6. 未見成交逾時與 R3
- `FILLS_WAIT_MS = 10_000`：交易所說 `Filled` 但看到的成交量不足時，`expected_at` 在第一次 poll 時記錄，每次成交計入後重設；`poll` 內的 `check_fills_wait` 超過就寫 `FILLS_NOT_SEEN` 並加失敗。poll 由每個交易事件與連線中的 `EquityTick` 驅動，實際延遲可能略長。
- 逾時後以已見成交量為準（`expected = filled`），後續階段照常進行。
- R3：`filled == 0` 時改用 `give_up_rest`（止盈階段放棄，止損仍是 `Todo`，之後若看到**該單自己 client id** 的成交仍送 closePosition 止損），並設 `reported`、推入 `finished`，清掉 attach 表。取捨：條目會留在 book 直到使用者處理（有失敗不被 housekeeping 移除），換來「晚到的成交仍有止損」。`Canceled`／`Expired`／`Rejected` 帶有未見成交時走同一條路。
- 有 client id 但沒有 trade id 的成交只寫 `log::info!`（不含 client id），不計入。

### 7. 系統通知（`src/notify_os.rs`）
- 只在保護失敗時、每個條目一次（`AutoEntry.notified`、`AutoBook::take_to_notify`），只在 macOS；其他平台不做事，面板橫幅就是通知。裸倉偵測屬 d5。
- 文字來自固定表（`Notice`），只帶 alias：`Account <alias>: protection failed. Open the account panel in Flowsurface to see what to do.`。alias 去掉控制字元、最多 32 字元、空白改為 `(no alias)`。
- AppleScript 是常數（`on run argv` … `item 1 of argv`），訊息以最後一個 argv 傳入，不拼進腳本；訊息一定以 `Account ` 開頭，不會被當成 osascript 選項。以絕對路徑 `/usr/bin/osascript` 啟動，不經 shell，stdio 為 null，由執行緒等待結束。啟動器以閉包注入，測試不啟動行程。
- 測試：`the_script_is_fixed_and_the_alias_is_only_the_last_argument`（含惡意 alias）、`the_message_is_the_fixed_text_with_the_alias`、`the_notification_comes_once_per_entry`。

### 8. 指紋限制（給 security-review）
- grant 內的「spec 指紋」是 `ProtectSpec` 的行程內 `DefaultHasher` 雜湊：只用來辨認同一次執行中的設定有沒有被改，**不是密碼學雜湊**，不存檔、不送出，重啟後無意義。它不能證明設定的來源，只能擋「確認後設定被換掉」；真正的授權邊界仍是 grant 的帳戶、symbol、方向、角色、進場 client id、期限與數量上限。

### 9. 自動路徑的 Unknown 紀錄
- 沿用 c2b-3 的 `RecordStatus` 與手動釋放流程；`AutoDone` 若帶有 Unknown 步驟就重新載入紀錄，面板才會顯示。測試 `an_unknown_result_of_an_automatic_order_follows_the_manual_release_flow`。

### 10. OKX 部分成交
- 有掛附屬單的 OKX 進場在部分成交期間，持倉列顯示 "Protection not active yet (entry protection pending, not in force yet)"（`Protection::EntryPending`，優先於清單上已有的止損）。測試 `a_partly_filled_okx_entry_says_protection_is_not_active_yet`。OKX 附掛表在完全成交後不會自動移除，是 d2／d3 既有問題，不在本段處理。

### 11. Live 開放前要在 testnet 確認
- Binance algo 撤單「不存在」的 code（`CANCEL_NOT_FOUND` 仍為空）。
- 同方向第二張 closePosition 是否回 -4130。
- 止損 `TRIGGERED` → `CANCELED` 的序列是否真的會發生。
- 報價過舊時（側欄隱藏）的實際行為與 330 秒是否合適。
- 重試的手動止損以 MARK_PRICE 為基準，與帳戶設定可能不同；確認視窗的欄位是否足夠清楚。
- macOS 通知在權限關閉或勿擾模式下的表現。

## M5 d5：孤兒偵測、保護標示、裸倉橫幅

d 段最後一段。沒有新的送單路徑、沒有新的權杖來源，`LIVE_SENDING_ENABLED` 仍為 false，`saved-state.json` 型別沒有變動。

### 1. T0／M1：「I've handled it」的確認文案
- `FailureView` 新增 `entry_open`（進場單可能仍在掛單：`sent == Stands` 且未完成）與 `standing`（該條目自己的保護單仍 `Open`／`ToCancel`／`Cancelling`／`CancelFailed` 的數量）。
- `account_panel::handled_warning` 依此補兩句：「The entry order is still open: a later fill will NOT get a stop-loss.」與「N protective order(s) of this entry may still stand on the exchange: cancel them first.」。單數用 "1 protective order"（偏離：簡報原文 N 一律複數）。測試 `the_handled_warning_says_what_stays_behind`、`the_handled_warning_facts_follow_the_entry_and_its_orders`。

### 2. 裸倉偵測（`TradingState::watch_bare`，每次 poll 呼叫，單調時鐘）
- 判定來源：`protected_status`（含 d4 的 `EntryPending` 覆蓋）。**只在 `lists_stale == false` 時下結論**；stale 時橫幅清空、寬限計時重來，面板顯示既有的 stale 橫幅與每個持倉的 "Cannot tell"，**不顯示已保護也不顯示裸倉**。
- 算作裸倉：`Unprotected`、`PartialStop`、`TrailingOnly`、`TargetOnly`。`TargetOnly` 為較低嚴重度（琥珀色橫幅、不發通知）。`EntryPending` 與帳本仍有工作（`counts_active`）的持倉**不**算裸倉（那是正常自動進場的空窗，持倉列已寫 "Protection not active yet"）；d4 已有失敗橫幅（`may_leave_bare`）的同帳戶、symbol、方向不重複顯示（`AutoBook::covers_part`）。
- **寬限 15 秒**（`BARE_GRACE_MS`）：Binance 自動進場在成交後送出止損，止損出現在清單上要再晚一點（algo 事件），這段空窗清單是新鮮的、持倉沒有止損，不能叫裸倉。連續 15 秒都在新鮮清單上無止損才確認。這是新增的偏離（簡報沒有寬限）。
- 被手動解除的 Unresolved 進場：條目已離開帳本，所以不再被 `covers_part` 覆蓋，持倉若沒有止損就會成為裸倉（滿足 d3 審查的說法）。
- 通知：每個（帳戶、symbol、方向）一次（`told`）；**只有新鮮清單判定部位消失或已有 `Stop` 才重設**，變 stale 不重設。同一輪同一帳戶合併為一則（`unique_accounts`）。文字固定、只帶 alias（`Notice::BarePosition`）。測試 `a_bare_position_is_confirmed_after_the_grace_time_and_told_once`、`going_stale_says_nothing_and_does_not_forget_who_was_told`、`a_position_that_is_gone_resets_the_notice`、`a_take_profit_only_position_is_shown_with_lower_severity_and_not_notified`、`a_pending_entry_does_not_hide_a_position_that_has_no_stop`、`a_stale_account_has_no_bare_rows_and_no_strays`；exchange：`bare_positions_are_the_ones_without_a_whole_stop`、`a_stale_list_has_no_bare_positions_and_no_orphans`、`hedge_legs_are_judged_one_by_one`。

### 3. 橫幅與按鈕
- `account_panel::bare_view`：「alias: the long/short position on SYMBOL has no stop-loss.」，按鈕「Add a stop-loss」與「Flatten (close all positions)」。
- 「Add a stop-loss」→ `Action::AddStop(symbol, dir)` → `main::start_add_stop`：以值再查一次 `bare_rows`，然後走 `open_stop_review` → `protect_attach::retry_stop`（要求持倉清單新鮮、持倉存在、帳戶預設有止損、報價不過舊）→ `start_review` → 確認視窗 → `confirm_order` → `order_flow::send`。新 preview、新 client id、新權杖；不產生 `AutoGrant`。「Flatten」沿用 `Message::CloseAll` → `Kind::Bulk(Flatten)`。掃描測試 `the_stop_and_cancel_offers_only_open_the_review_and_send_nothing_themselves` 鎖住四個函式不得出現 `order_flow::send(`、`confirm_order`、`.submit(`、`submit_auto(`、`AutoIntent::new(`、`auto_flow::mint(`、`AutoGrant`、`for_bulk(`、`BulkAction::CancelAll`。

### 4. 保護標示
- 每個持倉下列出保護單（`protecting_orders` + `describe_protective`）：止損、止盈、止損＋止盈同一張、追蹤，含觸發價與數量（全平型寫 "whole position"）。**只在清單新鮮時列出。**
- 文案：限價型止損／止盈（Binance `STOP`／`TAKE_PROFIT`，OKX 委託價不是 `-1`）寫 "then a limit order at P (it may not fill)"，讀不到價格就明說；追蹤寫 "Trailing stop (not counted as a stop-loss)"，帶 callback 與啟動價，讀不到就寫 "callback unreadable"、"no activation price read, so it may not be active"。欄位讀不到一律寫 unreadable，不猜。`OpenOrder` 新增 `limit`、`limit_price`、`activation_price`、`callback_rate`；欄位名稱（`price`、`activatePrice`／`activationPrice`、`callbackRate`；OKX `slOrdPx`／`tpOrdPx`、`activePx`、`callbackRatio`）**未對 testnet 核對**。測試 `every_protective_role_has_a_line_and_unreadable_fields_are_said`、`client_ids_and_trailing_fields_are_read_per_venue`、`protective_orders_are_listed_per_position_only_on_fresh_lists`。

### 5. 孤兒保護單與殘留自動單
- 孤兒：`orphan_protectives`（只在新鮮清單）。新增：讀不到 `positionSide` 的列（被當成 one-way）在該 symbol 持有 hedge 腿時**不算孤兒**，也不保護 hedge 腿（測試 `an_unreadable_position_side_neither_protects_a_hedge_leg_nor_is_an_orphan`），避免把真的 hedge 止損當孤兒提示撤銷。
- 殘留自動單：algo 單、client id 形狀像 app 的（角色字母 `e/s/t/r` + 20 個 base62，`looks_like_app_id`），而且既不在 `AutoBook`（條目鍵或 `by_placed`）、也不在本次執行中使用者確認過的請求 id（`sent_ids`，記憶體、上限 512）。文案寫 "looks like"：只是形狀比對。**OKX 不判定**（進場附掛的 algo 單帶 app 形狀的 id 且不在帳本內）。
- 「Cancel」**不使用** `BulkAction::CancelAll`（會撤其他持倉的止損），也**不新增權杖種類**：每張單走 `ManualSpec::Cancel` → `start_review` → 一般確認視窗（偏離：簡報寫「bulk 確認視窗同型」，改為逐單的一般確認，因為 bulk 權杖只有 `CancelAll`／`Flatten`，新增種類需要 exchange 的新權杖）。點擊時檢查（新鮮清單、仍是 stray、可依 id 撤）；確認時再檢查一次（`has_stray_mark` + `is_stray`），不符就不送。`OpenKind::Algo(None)` 的 OKX 單與 order list 不提供按鈕（`cancellable_by_id`）。
- `OpenOrder.client_id`：Binance `clientOrderId`／`clientAlgoId`，OKX `clOrdId`／`algoClOrdId`；空字串視為沒有；`Debug` 遮蔽（`a_debug_print_of_an_open_order_hides_the_client_id`）。欄位名稱以 stream 的解析為佐證，REST 列表的欄位**需 testnet 確認**；讀不到就是 `None`，各處以保守方式處理。
- d3 M4-4（偏離 1）：`ReconnectView.resting` 多帶進場 client id；有 client id 的列只在相同時才算「仍在掛單」，沒有 client id 的列維持 symbol＋方向比對（測試 `a_reconnect_matches_the_open_entry_by_client_id_when_the_row_has_one`）。

### 6. c2b3 d 段沿用
- hedge `positionSide` 解析失敗：測試 `a_position_side_that_cannot_be_read_gives_no_leg`（exchange）與上述孤兒測試。
- OKX `tdMode`：本 app 送出的 OKX 單固定 cross。快照讀到該 symbol 持倉為 isolated 時，`start_review` 拒絕（非撤單），文案說明沒有送出。只有已知的 isolated 才算不符；事件產生的持倉沒有 margin（未知）不算不符；位置事件保留快照讀到的 margin。撤單沒有 `tdMode`，永不因此被擋。測試 `okx_isolated_margin_is_a_mismatch_but_an_unknown_mode_is_not`。

### 7. 需要 testnet／手動
- `closePosition=true` 的條件單在無持倉時的行為（條件 66）。
- `openAlgoOrders`／`orders-algo-pending`／`orders-pending` 的 client id 欄位名稱，以及限價型止損、追蹤單的欄位名稱。
- 限價進場 → 關閉 app → 成交 → 重開：橫幅出現（15 秒寬限之後）、macOS 通知一次；主視窗與 popout 各驗一次。
- 手動：孤兒「Cancel」開啟確認視窗、LIVE 延遲、確認後清單更新；OKX isolated 持倉被拒的文案。
- 未做（Live 前待辦沿用）：T1 per-symbol 即時價。

### M5 d5 補充（審查後修正）
- **裸倉判定看清單，不看 `EntryPending` 覆蓋**：`TradingState::protection_listed` 只依持倉與掛單清單；`EntryPending` 仍寫在持倉列，但不會讓裸倉橫幅消失。只有 d4 已有 `may_leave_bare` 失敗的部位才不重複顯示。待成交的限價單、OKX 部分成交，都在 15 秒寬限後照常判定為裸倉並通知一次。
- **計時**：`Message::AutoPoll` 每秒觸發 `run_auto_jobs`，安靜帳戶重啟後寬限期也會走完。清單變 stale 時只隱藏橫幅，不重置寬限計時；只有新鮮清單判定部位已有止損或消失才丟掉計時。
- **殘留自動單（審查 B1 修正）**：重啟後 app 先前放的止損仍會被列為殘留，但若它是仍持有部位的止損（`protects` 成立且角色有止損，`exchange::trade::stop_guarded_position`），**不提供「Cancel」**，只顯示資訊列（"keep it unless you replace it"）；點擊與確認時的 `is_stray` 以同一規則重判，確認時若已成為保護唯一止損即拒絕。其餘殘留單（止盈、部分量止損、追蹤）仍可撤，該 symbol 有持倉時文案加註 "Cancelling it changes the protection of the {symbol} position"。測試 `strays_are_orphans_and_unowned_orders_of_the_apps_shape`、`a_stop_that_guards_a_held_position_is_not_a_stray_to_cancel`。
- **B2**：`open_stop_review` 在 `quote_of` 前先 `modal::order_ticket::valid_symbol`（≤28），`quote_of` 自己也檢查並回 `None`，避免交易所給的 29–32 字元 symbol 讓 `Ticker::new` panic。測試 `a_symbol_over_the_ticker_limit_is_not_valid`、掃描測試 `the_stop_and_cancel_offers_only_open_the_review_and_send_nothing_themselves`（檢查順序）。
- **順手項**：S1 Unresolved 進場在「I've handled it」補一句進場單可能仍掛著（`FailureView.entry_unresolved`）；S2 單數文案「cancel it first」；S3 更正 `watch_bare` 註解；S5 確認視窗關閉時清掉 stray 標記（`clear_stray_marks`）；S6 `client_id` 超過 64 字元視為沒有（`MAX_CLIENT_ID_LEN`）；S8 字串改用 `\` 續行，測試斷言文字不含連續兩個空白。
- **S7（只記錄）**：OKX `slOrdPx = -1` 搭配限價 `tpOrdPx` 的組合不會標成限價，只影響顯示文案。
- **Flatten**：`bulk.rs` 對每個持倉用該持倉自己的 margin，不經 `margin_conflict`；該檢查只擋非撤單的 `start_review`。
- **OKX「Add a stop-loss」**：OKX 永續（swap）以 `closeFraction=1` 的 order-algo 全平止損；OKX 現貨沒有手動保護單（`Unsupported`），按下後由 `retry_stop`／`build_intent` 顯示拒絕原因，不送單。

## M5 f0：DOM 點價與拖曳改單的共用地基（沒有使用者可見的新功能）

本段只鋪地基，不新增送單點，`LIVE_SENDING_ENABLED` 仍為 false，`saved-state.json` 沒有新欄位。`exchange/` 只在 f2-2 補了兩個唯讀欄位（見下），沒有碰送單、撤單、builder、閘門、grant、簽章、host 白名單、proxy、Keychain。

### 使用者決定（2026-11-12，後續段落依此實作）

- **DOM 點價**：Cmd（macOS）／Ctrl＋左鍵下**限價單**（bid 側買、ask 側賣）。數量取下單票據目前的數量，沒有就拒絕。單擊永不下單。不做市價與觸價（f1 處理）。
- **帳戶**：沿用下單票據目前選的帳戶。venue／product 與 pane 市場不符時，符合的帳戶只有一個就自動用，多個就請使用者先在票據選，**不自動猜**。pane 標題列顯示帳戶名與 TESTNET／LIVE。帳戶不存進 pane 設定或 `saved-state.json`。
- **拖曳改保護單（f3）**：一個確認涵蓋「撤舊＋下新」（exchange 新增 `ReplaceProtective` path 與 grant，需 security-review）。一律先撤後下並顯示裸倉警示；「先下新」做成每交易所旗標、預設關閉。只拖止損／止盈，不拖追蹤與進場單。Escape 取消拖曳。百分比／對數刻度不允許拖曳。

### f0 做了什麼

1. **確認視窗開在發起視窗。** `PrepareInput.origin` → `Prepared.origin`，`PendingConfirm::new` 用它；`BulkPrepared` 事件帶 window，`PendingBulk::new` 用它。送出時 `order_requests` 連同視窗一起記，`RulesExpired` 重新預覽沿用同一個視窗。票據與帳戶面板是主視窗的側欄選單，所以由它們發起的一律是主視窗；pane 發起的用 pane 自己的視窗（主視窗、popout、detached layout 三種）。`PrepareRequest` 本身沒變，所以請求相等性與既有簽章不受影響。
2. **路由。** `pane::Effect::TradeRequest` → `dashboard::Event::TradeRequest { window, request }` → `Flowsurface::start_trade_request`，只呼叫 `start_review`（開 review，不送單）。目前沒有 pane 會產生它。
3. **拒絕要讓使用者看到。** `start_review` 的所有拒絕改走 `review_refused`：照舊寫在票據上，票據沒開時另外跳 toast。
4. **帳戶解析純函式** `order_flow::resolve_pane_account`：只依規則選，不猜。選到的帳戶必須已測試，Live 帳戶還要 Live 開放。拒絕原因為 `AccountRefusal`（不可交易、沒有帳戶、需在票據選、未測試、Live 未開放），各有英文說明。
5. **`TradeOverlay`** view-model 空殼接進 `Dashboard::view`／`view_window`（三個呼叫處）。不 `Serialize`，由 `Flowsurface.trade_overlay` 持有。
6. 新發起函式 `start_trade_request` 加進掃描測試清單：只開 review，不得出現 `.submit(`、`submit_auto(`、`AutoIntent::new(`、`auto_flow::mint(`、`confirm_order` 等。

### 給 security-review 的重點

- 送單點未增加：`.submit(` 仍只在 `order_flow.rs`，`submit_auto(`／`AutoIntent::new(` 仍只在 `auto_flow.rs`。
- 條件 51 仍只擋進場；拖曳改單的「非 Connected 禁止」判定留給 f3（含 EX 端旗標）。
- 兩處 `#[expect(dead_code)]`（解析函式與其訊息文字）只在非測試建置生效，f1 開始使用時編譯器會要求移除。

## M5 f1：DOM 點價下單

不新增送單點：`.submit(` 仍只在 `order_flow.rs`，`submit_auto(`／`AutoIntent::new(` 仍只在 `auto_flow.rs`，`LIVE_SENDING_ENABLED` 仍為 false，`exchange/` 沒有改動，`saved-state.json` 沒有新欄位。

### 條件 80 已實作的部分

- **只有 Cmd（macOS）／Ctrl＋左鍵會發起下單。** `ladder.rs` 的 `press_message` 是純函式：修飾鍵＋左鍵落在有價格的列才產生 `Message::PriceClicked`；其他按下（單擊、Shift、右鍵、中鍵）一律維持既有的 `ResetScroll`；修飾鍵＋左鍵落在沒有價格的列（idx 0 的價差列）什麼都不做。修飾鍵判定與 kline 共用 `widget::is_command_modifier`。單擊永不下單。
- **列座標 → 價格**是純函式 `row_at(y, scroll, height, grid)`（含捲動、idx 0 無價、最佳價移動），價格轉成精確的 `Decimal`（`order_flow::price_to_decimal`，不經浮點）。
- **取整在交易所 builder，不在 GUI。** 限價價格由 `exchange/src/trade/{binance_futures,binance_spot,okx}.rs` 的 builder 以 `round_limit_price(side, …)` 取整（買向下、賣向上，用交易所 tick，不是 `PriceStep` 顯示分組）。確認視窗與偏離檢查看到的是 builder 預覽裡取整後的價。GUI 不再取整，避免重複或遺漏。
- **數量**取票據目前的數量（`OrderTicket::quantity`），沒有就拒絕並用 toast 說明；不存在 pane。
- **帳戶**用 `resolve_pane_account`（票據帳戶符合就用，否則只有一個符合才用，多個就拒絕），拒絕原因以 toast 顯示。ladder 標題列顯示解析到的帳戶名與 TESTNET／LIVE，或原因的簡短字樣（`TradeOverlay::account_tag_of`）。
- **會立即成交**：`Notes.takes_liquidity`（買價 ≥ 現價、賣價 ≤ 現價，現價為 ticker 表的 mark price）時，確認視窗顯示 "Takes liquidity"；既有 5% 偏離檢查照舊。
- **條件 51** 照舊由 `blocked_reason` 擋進場，點價走同一個 `start_review`，拒絕以 toast 顯示。
- **視窗關閉**：app 處理 `window::Event::Closed`（`window.rs` 映射，`main.rs` 呼叫 `forget_window`），所以不論哪條路徑關閉非主視窗（使用者關閉、detached layout、切換 layout 時關 popout、`merge_pane` 合併 popout）視窗消失後都會移除 scale factor 並清掉開在該視窗的確認（同 Escape 的 `discard_order`）；`forget_window` 可重複呼叫。`Action::Select` 在關 popout 前另外只清開在這些 popout 的確認（`confirm_lost_with_any`），不提早移除 scale factor。預覽完成時若發起視窗已不在，確認退回主視窗（`live_origin`），`RulesExpired` 重送亦同。`discard_order` 只移除該確認那一筆 `order_requests`（`forget_request`），不影響其他在途的送單。
- **雙擊保護（S0）**：pane 點價開出的確認（`OrderSource::Dom`）在 testnet 也等 `DOM_DELAY`（400 ms）才接受 Confirm，LIVE 取 700 ms 與 400 ms 較大者（700 ms）。來源在 `RulesExpired` 重送時保留。
- **深度檢查（S1）**：ladder 沒有任何深度（bid 與 ask 最佳價都不存在）、只有單邊，或深度超過 `DEPTH_MAX_AGE`（5 秒）沒更新時，Cmd＋點不產生請求（靜默不動作，沒有 toast）。沒有 mark 價時確認視窗的 "No market price…" 以警告樣式顯示。
- **拒絕與提示（S2、S3）**：有確認開在別處時，新預覽被丟棄改走 `review_refused`（票據關著時為 toast）；點價取代票據正在準備的預覽時 toast 提示。
- **確認視窗來源（L3）**：點價開出的確認多一行 "From the order book (DOM)"；帳戶名稱與 TESTNET／LIVE 沿用確認視窗既有的標題與帳戶行。

### 尚未做（留給後續段）

- 點價列的暫時高亮（沒做，成本不低：ladder 的畫面是快取的，要另加一層）。
- 不做市價與觸價單；不做 kline 上的點價。
- f3 的拖曳改單與 EX 的 `ReplaceProtective` 不在本段。

## M5 f2：圖表與 DOM 顯示掛單、持倉、保護單（唯讀）

不新增送單點：`.submit(` 仍只在 `order_flow.rs`，`submit_auto(`／`AutoIntent::new(` 仍只在 `auto_flow.rs`，`LIVE_SENDING_ENABLED` 仍為 false，`saved-state.json` 沒有新欄位。`exchange/` 只新增兩個唯讀欄位 `OpenOrder.price`、`Position.entry_price`（見 f2-2），解析結果只用於顯示，沒有碰送單、撤單、builder、閘門、grant、簽章、host 白名單、proxy、Keychain。線不能被抓取、拖曳或點擊，這一段沒有任何會改動訂單的操作。

### 做了什麼

- **view model**（`src/order_lines.rs`）：`OrderLine { price, kind, side, qty, whole, stale }`、`LineKind`（Entry／Stop／Target／Trailing／Position）、`SymbolLine`。純函式 `lines_of(orders, positions, stale)` 由 `OpenOrder`／`HeldPosition` 產生；沒有 `Serialize`、不含訂單 id 與 client id。
- **哪些列會畫**：帶 `role` 的條件單（algo），且 `side` 與正價位（trigger；追蹤止損才可用 `activation_price`）都讀得到才畫，缺欄位一律不畫、不猜。減倉判定（`closePosition`／`reduceOnly`，hedge 用 `positionSide` 的平倉邊）決定 Stop／Target／Trailing；不減倉的條件單畫成 Entry。OKX 帶 `attachAlgoOrds` 的進場單（`attached`）沒有 `role`，不畫、不當成保護中。只有 trigger 沒有數量的 algo 照畫（標籤寫 `all` 或不寫數量）。`StopAndTarget` 只有一個 trigger，畫成 Stop。一般限價單（`OpenKind::Plain`、沒有 `role`、讀得到 `OpenOrder.price` 與 `side`）畫在限價上：不減倉畫成 Entry；減倉（hedge 用平倉邊，one-way 用 `reduceOnly`／`closePosition`）畫成 Target，因為它是在指定價位出場的單；order list（`List`）與讀不到價格的列不畫。
- **帳戶與 symbol**：`TradeOverlay.lines`（每帳戶一份，`refresh_trade_overlay` 由 `TradingState` 重算）；pane 以 `TradeOverlay::lines_for(market, symbol)` 取線，帳戶用 `resolve_pane_account`（解析失敗、多帳戶未選、未測試、LIVE 未開放都不畫），symbol 為 `ticker.to_full_symbol_and_type().0`。帳戶不存進 pane 設定。
- **Kline**：`OverlayView::paint_order_lines`（`widget/chart/kline/overlay.rs`，在 Overlay 層）畫水平線與靠右的標籤（種類、數量）；顏色取自 theme palette（Stop＝danger、Target＝success、Trailing＝danger.weak、Entry＝primary、Position＝secondary.strong）。`KlineChart::set_order_lines` 比較後才 `render_invalidation.invalidate()`。
- **Ladder**：`Ladder::set_order_lines`、`lines_on_row`、`draw_order_marks`：與深度相同的分組規則（bid 向下、ask 向上，`round_to_side_step`），價格以 `Price::from_units`（`OrderLine::price_units`，無浮點）換算，所以剛好落在 step 上的價不會被推到隔壁列。在價格欄左緣畫色條，標籤畫在該側數量欄的內側。
- **條件 51**：`stale = TradingState::lists_stale(id)`（非 Connected、簿未受信、需重測、algo 事件後未重讀任一為真）。stale 的線改成虛線與淡色，標籤加 `· stale`；Ladder 標記同樣淡色並帶 stale。
- **效能**：overlay 在每次 `update`（非行情）重算，包括 `Message::Tick` 也會觸發訂單線重算；但每個 pane 只在線的內容改變時才失效快取（`set_order_lines` 先比較）。行情事件本身不重算。
- **`HeldPosition.entry_price`**：持倉快照（`Position.entry_price`）與持倉事件（`PositionUpdate.entry_price`）都會帶入；事件到達時覆寫快照的值，快照則整份取代。零、負值或讀不到都是 None，None 時不畫 Position 線。只用於顯示，不參與任何判定。

### f2-2：限價掛單價格與快照持倉均價（`exchange/src/trade/client.rs`，唯讀解析）

- `OpenOrder.price: Option<Decimal>`：Binance 一般單 `type` 為 `LIMIT`／`LIMIT_MAKER` 時讀 `price`（合約與現貨共用解析）；OKX 一般單 `ordType` 為 `limit`／`post_only` 時讀 `px`。市價、條件單（含 `STOP` 等的限價腳）、缺欄位、零、負值、無法解析一律 None，不讓整列失敗。`Debug` 不印此欄位，client id 仍遮蔽。
- `Position.entry_price: Option<Decimal>`：Binance 合約 `positionRisk.entryPrice`、OKX `positions.avgPx`；現貨沒有，為 None；零或無法解析為 None，不讓整列失敗。
- 測試：`plain_price_tests::{binance_futures_plain_limit_rows_carry_their_price, binance_spot_plain_limit_rows_carry_their_price, okx_plain_limit_rows_carry_their_price, position_snapshots_carry_the_average_entry_price, debug_of_an_open_order_still_hides_the_client_id}`；`order_lines::tests::{a_plain_limit_that_adds_is_an_entry_at_its_price, a_plain_limit_that_reduces_is_a_target, a_plain_row_without_a_usable_price_or_side_is_not_drawn}`；`trading::tests::a_snapshot_entry_price_is_held_and_an_event_can_replace_it`。
- 這些欄位只用於顯示，不進入任何送單、風控或保護判定；沒有新增 `saved-state.json` 欄位。

### 條件 78（擴充）

`order_lines::tests::order_lines_never_reach_the_saved_state`：`OrderLine`／`SymbolLine`／`LineKind` 不可序列化（編譯期探針）；序列化整個 `data::State` 不含訂單 id、價位或 `stale`；`data/src` 沒有任何檔案提到 `OrderLine`／`order_lines`（所以也進不了 `layout::drawings::Library`）。

### 已知限制

- 現貨沒有持倉均價（也沒有持倉），不畫 Position 線。
- 持倉事件若不帶入場價，會以 None 取代快照的值（不沿用舊值，因為加倉後舊均價可能已不對）；下一份快照會再補上。
- 同一價位的多條線在 kline 上互相覆蓋，沒有避讓；Ladder 同列的標籤合併成一行。
- 追蹤止損若只有啟用價，線畫在啟用價並寫 `trailing`。

### f3 待辦（security-review-f2 非阻擋項）

- saved-state 測試的 layout 沒有 pane，等於恆真；要改成含 pane 的 layout 再序列化。
- OKX 數量的單位是張數，畫面沒有標示；要在標籤上註明單位或換算成幣本位。
- 現貨的賣出限價單目前畫成 Entry；要依 side 與持倉狀態重新分類。

### 給 security-review 的重點

- `src/order_lines.rs`（全檔，純函式與測試）、`src/trading.rs` `HeldPosition.entry_price`／`apply_position_event`、`src/main.rs` `refresh_trade_overlay`、`src/screen/dashboard.rs` `lines_for`／`apply_trade_overlay`。
- 沒有新的 `Serialize`、送單點或 `exchange/` 變動。

## M5 f3a：改保護單的授權路徑（`exchange/src/trade/replace.rs`，只有 exchange，GUI 在 f3a2）

本段只做 exchange 端的授權與閘門：不改 builder 的取整與簽章、host 白名單、https_only、redirect、proxy、Keychain，不新增 GUI 送單點（`src/` 沒有改動），不新增 `saved-state.json` 欄位，`LIVE_SENDING_ENABLED` 仍為 false。

### 使用者決定（沿用 f0）
一個確認涵蓋撤舊與下新；一律先撤後下，撤單被交易所確認後才下新單；中間有裸倉，GUI 要警示。「先下新後撤舊」是 f3b 的每交易所旗標，本段不做。

### 授權綁定（`ReplaceGrant`）
- 由**使用者對「撤舊單、下新單」的那一次確認**產生：`TradeClient::replace_grant(token, account, &old_row, &fresh)`（到期時間用 client 自己的時鐘，呼叫端不能傳時間；`ReplaceGrant::new` 只有 crate 內可見）。`token` 必須是 `ConfirmToken::for_replace(&new_preview, &old_row)`，它同時綁新單的 preview 雜湊與舊單列的指紋（交易所 order id、kind、role、side、position side、數量、全平旗標；`Binding::Replace`，f3a2 的 N1 由只綁 id 收緊為整列指紋）。一般下單確認（`for_preview`）與批次確認都不是改單確認（`NotAReplaceConfirmation`），反過來改單確認也不能走 `submit`（`ConfirmationMismatch`）。`replace_grant` 會比對傳入的舊單列與 token 內的 id，不符回 `OldOrderMismatch`。
- 確認綁定：新單 preview 雜湊與舊單列指紋（見上）；grant 另外依傳入的舊單列檢查新單：種類（Stop 或 Target）、side、position side、數量或「全平」，並綁定帳戶與 symbol。舊單必須是 `OpenKind::Algo` 且 `role` 為 `Stop` 或 `Target`；追蹤止損、`StopAndTarget`、一般單、現貨一律 `NotReplaceable`；side 或數量讀不到、id 不是純數字則 `OldOrderUnreadable`。範圍：Binance USDⓈ-M 與 OKX 永續（OKX 只限 `conditional` 型）。
- **撤單由 grant 自己建**（Binance `DELETE /fapi/v1/algoOrder`、OKX `cancel-algos` 單筆），GUI 不傳撤單請求。要撤哪一張由使用者確認綁定的舊單 id 決定：grant 只能撤 token 所指的那一張，傳入別張、或同一張但 id 以外欄位（kind、side、position side、數量、全平旗標）讀得不同的 `OpenOrder` 會被 `OldOrderMismatch` 拒絕。（舊版這裡寫「不可能撤到別的單」並不精確：當時舊單 id 不在確認範圍內。）
- 到期：`REPLACE_GRANT_VALID_MS = 30_000`（確認視窗按下確認後立刻交給 `submit_replace`，半分鐘只容納慢的 gate 或限流）。只在撤單送出**前**檢查；撤單成功後不再因 grant 到期而放棄下新單（那會造成裸倉），新單仍受自己的規則年齡（`valid_until_ms`）限制。
- 單次使用：`submit_replace` 以值取走 grant（不能 `Clone`，有 `compile_fail` doctest）；送出過程中即使 future 被丟棄也不能再用。

### 順序與閘門（`TradeClient::submit_replace`）
1. grant 檢查（到期、帳戶、symbol、新單雜湊、新單為 `Reduces`、撤單仍是 grant 建的那一筆）與新單形狀檢查（見下）。
2. **兩個 leg 先全部通過 `approve`**（`Spent::replace(Cancel|Place, hash)` 只涵蓋各自雜湊的請求；F2 gate、風控、規則年齡、串流狀態）才送出任何請求。所以「可預見的」拒絕（gate、風控、串流、規則過期）不會發生在舊單已被撤之後。兩個 leg 都以 `GateClass::Reduce` 問 F2 gate。
3. 送撤單。只有 `Outcome::Accepted` **且回傳的 id 含舊單 id** 才算撤單成功（OKX 缺 `algoId` 的 `sCode 0` 不算）。`Rejected`（含交易所回找不到單，也就是舊單已觸發或已成交）、`Unknown`、`Partial`、`NotSent` 一律作廢，**不送新單**。
4. 才送新單。新單被拒或 Unknown 時舊單已不在。

### 拒絕條件與錯誤（`ReplaceError<R>`）
- `Refused(ReplaceRefusal)`：沒送任何請求，舊單還在。新單雜湊不符（`NewOrderMismatch`）、種類不同（`WrongKind`）、side 不同（`WrongSide`）、position side 不同（`WrongPositionSide`）、不是只減倉（`NotReducing`）、大小改變（`SizeChanged`：全平換成部分、部分換成全平，或數量大於舊單）、沿用舊單 client id（`SameClientId`）、過期（`Expired`）、帳戶或 symbol 不符、撤單被換掉（`CancelMismatch`）、確認的舊單與傳入的舊單列不同（`OldOrderMismatch`）。新單是進場單（加倉）或方向相反的保護單都落在這一類。
- `NotSent(SubmitError<R>)`：gate、風控、串流、規則過期、帳戶不符等，沒送任何請求，舊單還在。
- `CancelNotConfirmed(Submitted)`：撤單已送出但沒被確認；沒有新單被送出。若 outcome 是 `Unknown`，或是 `Accepted` 但回應不含舊單 id（交易所撤掉了某一張單，多半就是舊單），舊單都可能已不在，GUI 要顯示「可能沒有保護」並立即重讀掛單；撤單沒有 lookup（`pending` 為 `None`）。
- `NewNotPlaced { cancel, new }`：**舊單已撤、新單沒下成**。這是 GUI 必須大聲警示的狀態（裸倉）。`new` 為 `Some` 時帶新單的 outcome，Unknown 時 `new.pending` 交給 `resolve_unknown`；為 `None` 表示新單沒有離開本機（`Locked` 或 `JournalUnavailable`）。
- 不重試：任何一步都不會自動重送。
- Debug：`Outcome::Unknown` 的 client id 在 `Debug` 輸出中遮蔽（Display 與邏輯不變），所以 `{:?}` 印 `ReplaceError`／`ReplaceDone`／`Submitted` 不會帶出 client id；GUI 仍不應把它們寫進 log。

### 條件 51（拖曳改單在非 Connected 時由 exchange 拒絕）
`approve` 對 `Spent::replace` 多檢查 `allows(stream, TradeAction::DragModify)`（原本就存在，只在 `Connected` 為真），兩個 leg 都檢查，所以 `Reconnecting`／`Resyncing`／`Unavailable` 連撤單都不會送。**沒有新增 `GateClass` 變體**：`data/src/config/trade_gate.rs` 對它做窮舉比對，改它超出本段授權；兩個 leg 對 F2 gate 都是 `Reduce`，F2 gate 對 Live 在 `LIVE_SENDING_ENABLED` 為 false 時一律拒絕（`data` 的 `can_send_with` 表格測試涵蓋 Live＋`Reduce`），因此新 path 對 Live 帳戶同樣被拒；exchange 測試只用一個拒絕的 gate 驗證「gate 拒絕時連撤單都不送」，不宣稱測了那個常數。

### `Spent::covers` 收緊（d 段既有缺口）
原本 `Kind::Auto => true`。現在：
- `AttachProtective`／`CancelSibling` 經 grant 授權的請求，確認綁定 grant 檢查當下的請求雜湊（`GrantedAuto.bound`），並限制該 path 的種類（attach 只能 `Reduces`、撤兄弟只能 `Neutral`），永不涵蓋加倉。
- 風控平倉（無 grant）與批次確認一樣只涵蓋非加倉請求（`Kind::Unbound`）。
- `submit_auto` 不再經過 `submit_auto_unbound`；`submit_auto_unbound(` 的掃描測試因此由 2 處收緊為只有 `bulk.rs` 1 處（更嚴格，不是放寬）。

### 給 f3a2（GUI）的介面
1. 拖曳放開後用既有 `build_intent` 路徑組新單（同種類、同 side、同大小，只改價）→ `OrderIntent`，產生 preview 給確認視窗；視窗另外列出被撤的舊單：`ReplaceGrant::cancel_preview_for(account, &old_row, &fresh)` 不需要 token，確認前就能取得撤單請求的 preview（與 grant 之後送出的那一筆雜湊相同）。
2. 使用者確認時：`client.replace_grant(ConfirmToken::for_replace(&new.preview(), &old_row), account, &old_row, &fresh)`，再 `client.submit_replace(&gate, &risk, &journal, grant, new).await`。建 grant 失敗（`ReplaceRefusal`）時沒有送任何請求。
3. 結果對應 d4 的 `FailReason`：`Refused`／`NotSent` → 沒動，舊單還在；`CancelNotConfirmed` → 撤單失敗（`Rejected` 多半是舊單已觸發或成交）或未知（`Unknown`，要重讀掛單）；`NewNotPlaced` → 裸倉警示，並走保護失敗狀態。
4. GUI 仍要在拖曳開始就停用非 Connected 的線（exchange 的檢查是最後一道，不是唯一一道）。
5. 給 f3a2 的待辦（GUI 端必做）：
   - GUI 送單點的掃描（`src/order_flow.rs`、`src/trading.rs` 的清單）要加入 `submit_replace(` 與 grant 建立點（`replace_grant(`、`ConfirmToken::for_replace(`）。
   - d3 job 進行中的保護單不可拖曳（或讓 job 得知新 id）：`AutoGrant` 以 client id 追蹤自己發出的保護單，改單後新單的 id 不在 grant 內，止盈成交後撤兄弟會撤到舊單（not found），新單成為孤兒單。
   - `NewNotPlaced` 要大聲警示（舊單已撤、倉位缺這一道保護）。
   - `submit_replace` 的 future 不可隨視窗關閉被丟棄：撤單 leg 沒有 write-ahead 記錄，撤成功後被丟棄會留下裸倉，要用不隨視窗取消的 task。
   - GUI 手動下單紀錄要記改單的兩個 leg（撤單、新單）的結果；風控記錄目前只記自動路徑。
   - Binance hedge 模式的部分數量保護單（無 reduceOnly）會被 exchange 以 `NotReducing` 拒絕，GUI 要提早停用這類線，不要讓使用者拖了才被拒。
   - 拖曳開始時就停用非 Connected 或 `lists_stale` 的線。
6. `src/` 與 `data/` 沒有對 `AutoPath`、`GrantRefusal` 新增變體，既有的窮舉比對不受影響。

### 需要 testnet／手動
Binance 與 OKX 對「撤單 not found」的實際錯誤碼與時序、撤單成功但新單因「立即觸發」被拒的實際行為，都還沒在 testnet 核對；測試用本機假伺服器。

### 給 security-review 的重點
`exchange/src/trade/replace.rs`（全檔）、`gate.rs` 的 `Kind`／`Spent::covers`／`approve` 的 `is_replace` 檢查、`grant.rs` 的 `GrantedAuto.bound`、`client.rs` 的 `submit_auto`、`gate.rs` 的 `Binding::Replace`／`for_replace`／`spend_for_replace`、`outcome.rs` 的手寫 `Debug`、`source_rules_tests.rs` 的新掃描。

## M5 f3a2：在 K 線圖上拖曳止損／止盈線改價（GUI）

### 流程
拖曳 → 放開 → 重讀該列並檢查 → 組新單 → 一個確認視窗 → 一次 detached task 先撤舊單、再下新單。

1. **可拖的線**：只有保護單的 Stop 與 Target 線（`order_lines::Grab::Yes(交易所 order id)`）。追蹤止損、進場單、Position 線、plain limit 線都不可拖。可拖的線帶交易所 order id，不帶 client id；線仍不寫入 `saved-state.json`，拖曳狀態（`kline::State.order_drag`／`order_hover`）只在 widget 記憶體內，不進 `drawings::Library`。
2. **停用條件**（`replace_flow::drag_block`，hover 顯示原因，游標為 NotAllowed）：stream 非 Connected、掛單／持倉清單 stale、LIVE 帳戶（live 關閉）、百分比或對數刻度、屬於仍在進行中的 d3 自動 job 的保護單、Binance hedge 部分數量保護單（無 reduceOnly，exchange 會以 `NotReducing` 拒絕）、OKX 非 conditional 單、限價觸發單、非合約產品、讀不出必要欄位的列、同帳戶已有一筆改單在送。
3. **放開後**（`main::start_replace_request`）：帳戶由 `resolve_pane_account`（f0）決定，不猜；用**最新讀到的**列（依 order id 重讀，不用畫面上那條線）重新判斷停用條件；`replace_flow::plan` 以相同種類、相同 side、相同大小（或全平）組新單，只改觸發價，價格經交易所 `round_trigger_price` 取整，新價（未取整）與原價相同則不動作（靜默）；取整後才與原價相同時，`replace_info` 回錯誤「rounds to the old trigger price」，以 toast 與單據狀態說明，不開確認視窗。Escape 或拖出圖表範圍取消；放開時位移小於 4 px 視為點擊，不改單。
4. **確認視窗**：列出「撤銷 X（trigger A）→ 新增 Y（trigger B）」，撤單內容來自 `ReplaceGrant::cancel_preview_for`；顯示帳戶、TESTNET／LIVE、來源 "Dragged on the chart"，並警示「先撤後下，中間會短暫沒有保護」（裸倉警示）。新價若會立即觸發（越過現價）另有警示，但不禁止，交易所會拒。沿用 f1 的延遲接受點擊（`DOM_DELAY`，與 DOM 相同）、origin 視窗路由與 `forget_window`。token 為 `ConfirmToken::for_replace(&preview, &old_row)`，只在 `modal/order_confirm.rs` 的 `confirm_replace` 產生。
5. **送出**（唯一的 GUI 改單送出點：`order_flow::send_replace`／`run_replace`）：確認時再次判斷條件 51 與停用條件（`replace_flow::recheck`，在最新清單上，列已消失、已改變、被擋都不送），再 `client.replace_grant(...)` → `client.submit_replace(...)`。不重送。
6. **不隨視窗丟棄**：`run_replace` 以 `tokio::spawn` 起一個 detached task；`JoinError` 回 `ReplaceResult::Lost`（視為結果未知，要檢查）。送出中狀態放在 `TradingState.replace`（`ReplaceFlow`），不在 `order_confirm` 內，因此關閉視窗、`discard_order`、`forget_window` 都取消不了它。
7. **結果處理**（`replace_flow::handle`，`main::on_replace_sent`）：
   - `Done`：線移到新價；GUI **不**標記清單，由 stream 在 algo 事件時自己標 dirty 並重讀（見 B1）。
   - `Refused`／`NotSent`：toast，線回原位（沒動）。
   - `CancelNotConfirmed`：`Rejected` 多半是舊單已觸發或成交；`Accepted`／`Unknown`（N4）可能舊單已不在、也沒有新單，顯示「可能沒有保護，請檢查」，不標記清單（見 B1）。
   - `NewNotPlaced`：舊單已撤、新單沒下成：d4 紅色橫幅（可 Retry protection 僅限 Stop、可 Flatten）＋ OS 通知；`new.pending` 交給 `resolve_unknown`；只有 `NewLeg::Unresolved` 與 `Lost`（任一 leg 可能已送出）時該 symbol 才維持封鎖並重載 Unknown 紀錄（`replace_flow::keeps_symbol_blocked`）。結果處理結尾與 `on_order_sent` 一樣呼叫 `run_auto_jobs()`。撤單 leg 沒有 pending：撤單請求（Binance DELETE、OKX cancel）的 `request.lookup()` 為 `None`，所以不寫 write-ahead 紀錄、也不產生 `PendingUnknown`，`CancelNotConfirmed` 不需 `resolve_unknown`。
   - 兩個 leg 的結果記成固定字詞的 `log::info!`（`ReplaceResult::tag()`），並顯示在 toast 與單據狀態；不 `{:?}` 記錄 `ConfirmToken`、`ReplaceError`、client id。

### 掃描與測試
- GUI 送單點掃描（`order_flow::tests::submit_is_called_from_this_file_only_and_the_automatic_paths_from_auto_flow_only`）新增：`submit_replace(` 與 `replace_grant(` 只能出現在 `order_flow.rs`；token 掃描允許 `for_replace(&self.preview, &info.old)` 只在 `order_confirm.rs`；`start_replace_request` 只開 review（`start_review_of`）。
- `replace_flow` 內的測試涵蓋各停用條件、hedge 部分數量與 OKX 非 conditional、讀不出的列、相同價不動作、重檢（消失／改變／被擋）、y → 價格 → 十進位、各結果處理、橫幅與通知、每帳戶同時一筆改單。
- 另有 `order_lines`（Grab、現貨賣單、OKX `ct`）、`layout`（`a_layout_with_a_kline_pane_saves_no_order_line`）與 `order_confirm`（Replace 來源與延遲）的測試。

### 與 f2 的差異（f2 的「線不帶 order id」已改）
f2 的線不帶 order id；f3a2 起只有可拖的 Stop／Target 線帶**交易所** order id（`Grab::Yes`），仍不帶 client id，仍不序列化。f2 留下的三件事：saved-state 測試新增有 kline pane 的 layout（pane 以 `chart: None` 建立，線無法直接掛上去，所以檢查的是線的內容不會出現在序列化輸出）；OKX 的數量單位在線標籤加上 ` ct`（張）；現貨賣出限價單畫成 **Target**（賣出已持有的資產，等於獲利了結；現貨買入限價單仍是 Entry）。

### 偏離與取捨
- 風險記錄不能擴充（`data/` 不在授權內），所以兩個 leg 的結果以固定字詞 `log::info!`、toast 與單據狀態記錄，沒有寫入風險記錄。
- **B1（security-review 必修，已修）**：原本 `on_replace_sent` 在結果後呼叫 `mark_orders_dirty`。stream 只在收到 algo 事件時才重讀，沒有定期重讀；撤單未送出或被拒（不會有事件），或快照早於結果到達（新單沒下成、之後沒有事件）時，dirty 標記永遠不會被清掉，`lists_stale` 一直為真，d5 裸倉監控與紅色橫幅的 Retry protection 都被靜默關閉。修法：**不在 GUI 標記**（刪掉 `mark_orders_dirty` 與 `Handling.reread`）。stream 為 Connected 時，每個 algo 變化都會送事件，事件本身在 `trading.rs` 標 dirty 並安排重讀；非 Connected 時 `lists_stale` 本來就為真。代價：結果與事件之間畫面仍是舊線，這段期間靠再次拖曳時的 `recheck` 與 exchange grant 的舊單列比對把關。測試：`a_finished_replace_never_leaves_the_lists_stale_so_retry_protection_stays_usable`（含掃描 `on_replace_sent` 不得碰 `orders_dirty`）。
- 沒有 GUI 的「立即重讀掛單」API（見 Live 開放前待辦）；N4 的「立即重讀」目前只靠 stream 的 algo 事件。
- **S1**：`Lost` 與 `Unresolved` 一樣維持 symbol 封鎖並 `reload_records`。**S3**：`on_replace_sent` 結尾呼叫 `run_auto_jobs()`。
- **S4**：改單進行中（`TradingState::replace_busy`）時，撤全部／全平不能開始或確認（`bulk_blocked_reason` 新增 `replacing` 參數，帳戶面板按鈕也一併停用），避免留下孤兒的 reduceOnly／closePosition 止損。測試 `a_deleting_account_or_an_open_confirmation_blocks_bulk`。
- **N1**：`Lost` 後若該帳戶與 symbol 沒有 Unknown 紀錄，alert 改寫為「重啟 app 後才會解除」（沒有紀錄可在 Order ticket 解除）；有紀錄時維持原文。沒有新增手動解除路徑。**N2**：`on_replace_sent` 對 `TradingState` 的處理抽成 `TradingState::on_replace_result`，測試直接呼叫它。
- **S7**：立即觸發的比較價是 `tickers_table.mark_price`，其值在 Binance、OKX、MEXC 是**最新成交價**（`last_price`），只有 Bybit、Hyperliquid 是真的標記價格。警示文字因此改稱 "ticker price"。
- 不動 `auto_flow.rs`：改單失敗以合成的 `FailureView` 在 `trading.rs` 併入 `auto_failures`（key 前綴 `replace-`），Retry 只提供給 Stop。
- `RulesExpired` 不自動重新 review，只顯示 Refused toast，使用者重新拖一次。
- 限價觸發單（`LimitTrigger`）不可拖：限價與觸發價的關係不是單一價位。
- `modal/account_panel.rs` 只為 S4 增加 `bulk_blocked_reason` 的 `replacing` 參數。

### Live 開放前待辦
1. stream「要求立即重讀」API（需要 `exchange` 授權）：GUI 才能在改單結果後主動重讀，完整做到 N4。
2. 撤單 leg 的 write-ahead journal：detached task 擋得住關閉視窗，擋不住 app 結束；撤單成功、新單送出前結束程式，磁碟上不會留下紀錄。目前也沒有測試實際執行 `run_replace`。
3. testnet 核對：Binance `DELETE algoOrder` 與 OKX `cancel-algos` 的回應是否帶 `algoId`、not found 的錯誤碼、立即觸發被拒的行為、-4130。
4. 手動實測 B1：`NewNotPlaced` 出現後，Retry protection 是否可用。

### 需要手動 `cargo run`／testnet 確認
拖曳手感與 hover 游標（Grab／Grabbing／NotAllowed）、預覽虛線與價格標籤、Escape 取消、拖出圖表取消、百分比／對數刻度的提示、確認視窗版面與裸倉警示、延遲接受點擊、testnet 上改單結果的 toast 與橫幅、macOS OS 通知、重讀後線移到新價。


## f3c 補充：自動掛上的止損／止盈也能拖曳（auto 認領替換單）

f3a2 把「book 認得這張單的 client id」當成「auto job 正在處理」，所以 d2／d3 自動掛上的保護單只要還掛著就不能拖；而 app 大多數保護單都是自動掛的。f3c 把停用條件改成「job 正在對這張單動作」，並在改單成功後讓 auto book 與 grant 改認新單。

### 停用條件（`AutoBook::is_working_on`，取代 `knows_client_id` 在拖曳上的用途）
一張單被視為「job 正在處理」，只要符合任一項：
- 它是 entry 本身（entry 單不是可改的保護單）。
- 該 entry 有 job 在途（`busy`：附掛或撤兄弟正在送出，job 會讀寫 placed 紀錄）。
- 這張單的 state 不是 `Open`：`ToCancel`、`Cancelling`、`CancelFailed`、`Triggered`、`Finished`、`Failed`、`Cancelled`。

由 app 掛上、`Open`、且沒有 job 在途的單視為穩定，可以拖。沒有 client id 的列仍用原本的「該 symbol 有 job 在運作」判斷。`drag_block`、`plan`、`recheck` 仍在確認時再判斷一次（`is_auto_order` 的呼叫端不變）。

### 認領（`AutoBook::adopt_replacement`）
只有 `ReplaceResult::Done` 帶 `ReplaceSummary.replacement`（`submit_replace` 兩個 leg 都被交易所接受才產生）；`NewNotPlaced`、`CancelNotConfirmed`、`Refused`、`Lost` 都是 `None`，不認領。`on_replace_sent` 在 `on_replace_result` 之後呼叫 `TradingState::adopt_replacement`：
1. 以舊單 client id 查 `by_placed`，不是 book 掛的單 → `NotOurs`，什麼都不做。
2. 舊單的 book state 必須是 `Open`、`ToCancel`，或 `Cancelled`（stream 的撤單事件比 HTTP 回應早到，這張單是被這次改單撤掉的）；`Cancelling` 等其他狀態拒絕。
3. 先唯讀比對：grant 對舊 id 的槽位（`AutoGrant::issued_ref_of`）必須等於 book 記的槽位，不一致就拒絕且 grant 不動；一致才由 grant 認領（失敗就整個不動，並寫一則 note：「被取代的止損／止盈不再由自動保護追蹤，請檢查掛單」，新單由 `orphan_protectives` 顯示）。
4. 成功後 placed 紀錄換成新 client id（`IssuedRef`、角色不變），`by_placed` 刪舊加新。新單的 state 依 sweep 規則重新判斷（`AutoBook::swept`，與 `triggered` 共用同一張 `sweep_of` 表）：若止損已觸發（或追蹤止損已觸發而新單是止盈），新單為 `ToCancel`，下一次 poll 撤新單（改單在途期間 book 不對舊 id 排撤單、也不移除 entry，見「改單在途的保留」，所以事件之間夾 poll 也成立）；否則 `Cancelled` 還原為 `Open`。這是因為撤單事件（`Cancelled`）可能在止損觸發之前或之後到，兩種順序都會讓舊單的 `ToCancel` 標記被蓋掉或被跳過，所以不能沿用舊單當下的 state。舊單之後的撤單事件不再屬於 book。
5. 認領成功後，重放暫存的新單事件（見下）。

之後止盈成交（`release_stop`）或止損觸發（`triggered`）撤兄弟時，`cancel_intent` 由 grant 的紀錄建請求，撤的是新單，不是舊 id。測試：`after_a_replace_a_finished_take_profit_cancels_the_new_stop`、`after_a_replace_a_fired_stop_cancels_the_new_take_profit_and_the_other_one`（請求數與 id 斷言）。

### exchange 的最小改動與授權為何沒有放寬
- `replace.rs`：新增 `Replacement`（`ReplaceDone.replacement: Option<Replacement>`）。欄位全是 `pub(crate)`，只有 `submit_replace` 在**取消被接受且點名舊單、新單被接受**之後才建立，GUI 無法自己組（`for_test` 只在 `test-support` feature）。內容：account、symbol、kind、舊 client id、新 client id。只有 Binance 且舊列有 client id 時才有；OKX 為 `None`（OKX 沒有 auto grant）。`Debug` 遮蔽。它是 `Clone`（`ReplaceSummary` 要 Clone），但它只是「已完成的事實」，不是權限：權限檢查都在 `adopt_replacement`。
- `grant.rs`：新增 `AutoGrant::adopt_replacement(&Replacement) -> Result<IssuedRef, GrantRefusal>`，把 grant 記錄中那張單的 id 換成新 client id，其餘不動。它檢查：帳戶與 symbol 相同；舊 id 是這個 grant 發出的；該單 state 是 `Sent`（未送出或已撤的拒絕）；角色與改單種類相符（Stop↔StopLoss、Target↔TakeProfit，不能跨角色）；新 id 是合法 client id 且 grant 從未見過。
- 為何沒有放寬：(1) grant 可撤的單數不變，只是同一個槽位從已不存在的舊單改成使用者確認過的新單；(2) 舊 id 在認領後不再能通過授權（再認領一次、或對舊 id 撤單，都找不到 id）；(3) 新單不會變成「可再下單的空位」，state 是 `Sent`，`AttachProtective` 對它仍回 `IdNotAvailable`；(4) 不改 `reduce_authorised`、不改可下單的角色或數量（改單路徑已拒絕比舊單大的新單）；(5) 憑證只能由完成的、經使用者確認（`for_replace` 綁舊列與新單雜湊）的改單產生，不能由 GUI 偽造；(6) 失敗時 grant 完全不變。`Spent::covers`、`GrantedAuto.bound` 不動：撤單請求仍由 grant 自己建、雜湊綁在該次請求上。
- `gate.rs`：`AutoIntent::client_ids()` 唯讀存取（測試用，不能組請求）。確認指紋 `OldRow` 現在也含舊列的 `client_id`：交易所 id 相同但 client id 不同的列會被 `OldOrderMismatch` 拒絕，避免認領把另一張單的槽位換掉。
- `grant.rs`：`AutoGrant::issued_ref_of(&str) -> Option<IssuedRef>` 唯讀，讓 book 在改動 grant 之前先確認兩邊對同一張單的看法一致（「先全檢查、再改」）。
- exchange 測試：`an_adopted_replacement_is_what_the_cancel_of_the_sibling_names`、`an_adoption_covers_only_the_confirmed_order_and_nothing_else`（反例：其他 id 仍被拒、舊 id 被拒、新 id 不能再下單、重複認領被拒）、`an_adoption_that_does_not_fit_the_grant_is_refused_and_changes_nothing`、`an_order_that_was_issued_but_not_sent_or_already_cancelled_is_not_adopted`；`send_tests` 的 `a_completed_replace_carries_the_proof_for_the_automatic_grant`、`a_replace_of_an_order_without_a_client_id_has_no_proof`。

### 新單事件早於回應（暫存重放）
`AutoBook` 每個帳戶保留最多 32 筆（`MAX_UNCLAIMED`）client id 不認得、形如 app id、且會改變狀態的 algo 事件（`New`／`PartiallyFilled` 不存）；只在記憶體，不寫 log、不進 saved-state，帳戶移除時清掉。認領成功後依序把 `new_client_id` 的事件重放進 `on_algo_update`，所以新單在回應前就觸發也會正確 sweep 兄弟單。測試：`a_new_order_whose_events_came_before_the_answer_is_handled_once_adopted`、`the_events_kept_for_unknown_orders_are_bounded_and_only_for_app_ids`。

### 改單在途的保留（R1）
app 在每個 private 事件之後都會 poll，所以改單回應回來之前，book 已經看過每個事件。若不保留，兄弟單（止損）在改單窗口內觸發會有兩種壞結果：舊單被標 `ToCancel` 後 poll 立刻對舊 id 排撤單（順序 B，認領被拒）；或舊單先被撤單事件標成 `Cancelled`，entry 被 `housekeeping` 移除（順序 A，認領回 `Dropped`，新單成為孤兒）。

做法：`AutoBook.replacing`（**舊單的 client id** → 帳戶）。book 的 key 一律是 client id，不是交易所單號（`OpenOrder.id` 與 `OpenOrder.client_id` 是不同欄位）：
- 開始：`main::confirm_replace` 呼叫 `TradingState::replace_begun(account, &OpenOrder)`，由 `AutoBook::replace_begun` 自己從列上取 `client_id`；列沒有 client id、不是 book 掛的單、或 entry 屬於別的帳戶時不記錄（只改 book 狀態，不新增送單點）。簽名收整列而不是字串，避免日後又傳錯 id。
- 結束：`on_replace_sent` 一開始、認領之前呼叫 `replace_ended(account)`，**依帳戶**清除（一個帳戶同時最多一個改單，`replace_flow.begin_send` 保證），所以不論結果（`Done`、`Refused`、`Lost`、task panic）、也不需要結果帶 client id，都一定清得掉；`remove_account` 一併清除。
- 標記存在時：`housekeeping` 不移除含該 id 的 entry；`poll_entry` 對該 id 的 `ToCancel` 不排撤單，維持 `ToCancel`；entry 有這種保留中的 `ToCancel` 時，`poll_entry` 在送出其他（非保留）單的撤單 job 之後直接回傳，不排任何 stage（維持先撤再掛，N5）。
- 結果是 `Done` 時由 `adopt_replacement` 接手，新單為 `ToCancel`，下一次 poll 撤新單；結果不是 `Done` 時標記已清，照原路徑撤舊 id 或由事件推進。

測試（用 `id = "9001"` 與 client id 不同的 `OpenOrder` 列）：`a_replace_is_held_back_by_the_client_id_not_the_exchange_id`（在途時 `replacing` 非空、不對舊單撤單、`Adopted`、新止盈被撤、ended 後為空）、`the_end_of_a_replace_clears_the_account_whatever_the_result`（非 `Done` 變體，ended 後舊 id 照原路徑被撤）、`a_replace_without_a_client_id_or_of_another_account_is_not_recorded`、`a_held_back_cancel_keeps_the_stages_from_being_placed`（N5）。測試在 `AutoBook` 層，經 `TradingState` 的兩個方法只是一行轉呼叫，`main` 呼叫的就是它們。

### entry 已被移除後的認領（S5）
標記只在 book 掛的單且 entry 還在時才記。entry 若在改單送出之前就已移除（例如更早就結束），book 記住最近 64 個（`MAX_FORGOTTEN`）被移除 entry 的 (帳戶, 單 id)，認領時回 `Adoption::Dropped`，`on_replace_sent` 跳出警告 toast「新單不再由自動保護追蹤」，並提醒「若倉位已平，請撤掉這張單」（不含 id）。`forgotten` 以帳戶為 key，`remove_account` 一併清除。測試：`an_order_of_an_entry_that_left_the_book_is_told_from_one_never_placed`、`a_forgotten_order_is_told_apart_per_account`。

### GUI 測試（`auto_flow::tests`）
`an_order_the_app_placed_is_draggable_while_it_stands_and_no_job_works`、`an_order_that_is_marked_cancelling_or_over_is_not_draggable`、`the_exchanges_cancel_event_that_comes_before_the_answer_does_not_lose_the_order`、`a_replace_that_was_not_completed_is_not_followed`、`an_adoption_the_book_or_the_grant_cannot_take_changes_nothing`、`an_adoption_never_lets_the_grant_cancel_an_order_it_did_not_send`；M1：`a_stop_that_fires_after_the_cancel_event_of_a_replace_takes_the_new_take_profit`、`a_stop_that_fires_before_the_cancel_event_of_a_replace_takes_the_new_take_profit`；S4：`a_grant_slot_that_the_book_names_differently_changes_nothing`。

### 已知限制
- 新單事件暫存只有每帳戶 32 筆；同一帳戶在改單回應前湧入大量其他 algo 事件時，最舊的會被擠掉，新單的事件就漏掉，book 之後一直把新單當成 `Open`、entry 不會被移除，`orphan_protectives` 與 d5 仍會顯示。
- 改單送出之前，d3 若已對舊單送出撤單（`Cancelling`），認領被拒並寫 note，新單成為孤兒單（reduceOnly／closePosition，不加倉）。改單在途期間不會再新增這種撤單（R1）。
- 改單送出前 entry 就已被移除時，認領回 `Dropped`：新單不被追蹤；若倉位已被平掉，使用者要自己撤掉這張單（toast 有說）。
- 改單在途期間，被改單的那張舊單的撤單、entry 的移除與後續 stage 的送出被延後，直到回應或 task 結束（其他兄弟單的撤單照常）。上限：改單路徑遇到 limiter pause（signed 429 最長 3600 秒）**不會等待**，`send_limited_meta` 立刻回 `LocalRateLimited`（`NotSent`），所以限流不會拉長在途時間；真正會等的只有結果不明時的查詢（`DEFAULT_SCHEDULE` 約一分鐘，加上 `recvWindow` 與 `SETTLE_BUFFER_MS`／OKX 60 秒底限的延伸）與每個請求 30 秒 timeout，正常最壞約數分鐘。**刻意沒有在 `send_replace` 外層加整體 timeout**：撤單 leg 沒有 write-ahead 記錄，逾時丟掉 future 會讓已送出的撤單結果消失、留下裸倉而不自知；寧可等結果。
- `send_tests` 的 Binance 改單測試證明 `ReplaceDone` 帶憑證；`run_replace` 到 `on_replace_sent` 的整段仍需 testnet 確認。

### 需要手動 `cargo run`／testnet 確認
自動掛上的止損與止盈線可拖（沒有 job 在途時）、拖曳後該線移到新價；之後讓止盈成交，確認新止損被撤而不是留成孤兒單；job 在途時線仍顯示「automatic protection is still working」。
