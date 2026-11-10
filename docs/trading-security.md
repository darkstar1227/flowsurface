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
    - M4 的實作方式：以 `src/connector/client.rs` 的 `signed_client` 建立，未經 `http_client(...)`。原因是 `http_client` 建立失敗會 panic，套用 proxy 失敗時會改為直連，兩者都與條件 21 衝突。此替代做法已審查通過，且達成條件 17 與 21 的目的。
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
- `src/connector/client.rs` 的 `signed_client`：簽名用 client（`redirect(none)`、`https_only(true)`、嚴格 proxy）。

## M5 前必做

- **F1（中）**：Binance 簽名請求（`exchange/src/account.rs` 的 `send_get`）沒有與公開 `/fapi` 請求共用 limiter。M5 下單與查倉會持續送出簽名請求，必須共用同一個 limiter 實例，並在每次送出前取得配額。
- **F2（中）**：判定結果目前只存在 modal 的 `Status`。M5 下單前必須讀取記憶體中的判定，確認 `is_usable(env, verdict)`。不得從 `saved-state.json` 讀取，也不得以「沒有測試過」當成預設可用。下單時要重新呼叫 `load_credentials`（會比對 venue 與 env），不要留存 Test 時取得的 `ApiCredentials` 重複使用。
- **F10 延伸**：M4 只在 Test 前檢查 proxy 是否變更（`signed_client_for_current_proxy`），變更後要求重啟。M5 必須把同一檢查延伸到每個簽名請求與私有 WebSocket。

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
