//! USDⓈ-M listenKey lifecycle calls (condition 49). The key is returned wrapped in
//! [`ListenKey`] and nowhere else; errors carry no key and no URL.

use super::secret::ListenKey;
use crate::account::AccountError;
use crate::trade::client::TradeClient;
use serde_json::Value;

impl TradeClient {
    /// Create (or fetch the already active, extended) listenKey of this account.
    pub(crate) async fn listen_key_create(&self) -> Result<ListenKey, AccountError> {
        let v = self.listen_key_call(reqwest::Method::POST).await?;
        v.get("listenKey")
            .and_then(Value::as_str)
            .and_then(ListenKey::parse)
            .ok_or(AccountError::BadResponse)
    }

    /// Extend the listenKey by 60 minutes. Called every `KEEPALIVE_EVERY`.
    pub(crate) async fn listen_key_keepalive(&self) -> Result<(), AccountError> {
        self.listen_key_call(reqwest::Method::PUT).await.map(drop)
    }

    /// Close the listenKey (account deleted or disabled).
    pub(crate) async fn listen_key_close(&self) -> Result<(), AccountError> {
        self.listen_key_call(reqwest::Method::DELETE)
            .await
            .map(drop)
    }
}
