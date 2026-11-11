//! The HTTP client for signed (credential-bearing) requests, together with the proxy it was
//! built with.
//!
//! The two halves cannot be separated or swapped: [`SignedHttp::build`] is the only way to
//! make one, and the proxy state it reports is the one its client really uses. A caller
//! therefore cannot pair a direct client with a `Proxied` snapshot (which would pass the
//! snapshot check while bypassing the user's proxy). The client never follows redirects,
//! speaks https only and fails closed on an unusable proxy.

use crate::account::{AccountError, ProxyState};
use std::fmt;
use std::time::Duration;

/// The client for signed requests plus the proxy it was built with.
///
/// Struct literals are impossible outside this module, and `TradeClient::new` takes this type
/// instead of a bare `reqwest::Client`:
///
/// ```compile_fail,E0451
/// use flowsurface_exchange::account::ProxyState;
/// use flowsurface_exchange::signed_http::SignedHttp;
/// // The fields are private: a client cannot be paired with a proxy state by hand.
/// let _ = SignedHttp { client: reqwest::Client::new(), proxy: ProxyState::Direct };
/// ```
#[derive(Clone)]
pub struct SignedHttp {
    client: reqwest::Client,
    proxy: ProxyState,
}

impl SignedHttp {
    /// Build the client for `proxy`. `Broken` is refused, and a proxy that cannot be applied
    /// is an error, never a direct connection.
    pub fn build(proxy: ProxyState) -> Result<SignedHttp, AccountError> {
        let builder = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .https_only(true);
        let builder = match &proxy {
            ProxyState::Broken => return Err(AccountError::ProxyBroken),
            ProxyState::Direct => builder,
            ProxyState::Proxied(p) => crate::adapter::proxy::apply_proxy_strict(builder, Some(p))
                .map_err(|_| AccountError::ClientBuild)?,
        };
        let client = builder.build().map_err(|_| AccountError::ClientBuild)?;
        Ok(SignedHttp { client, proxy })
    }

    /// The proxy this client was built with, read-only (the GUI compares it with the current
    /// setting).
    pub fn proxy_state(&self) -> &ProxyState {
        &self.proxy
    }

    pub(crate) fn client(&self) -> &reqwest::Client {
        &self.client
    }

    /// A client for tests that talk to a local server, with the proxy state the test names.
    #[cfg(test)]
    pub(crate) fn for_test(client: reqwest::Client, proxy: ProxyState) -> SignedHttp {
        SignedHttp { client, proxy }
    }
}

/// Prints the kind of proxy only: never its address or credentials.
impl fmt::Debug for SignedHttp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.proxy {
            ProxyState::Direct => "direct",
            ProxyState::Proxied(_) => "proxied",
            ProxyState::Broken => "broken",
        };
        f.debug_struct("SignedHttp").field("proxy", &kind).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::{Proxy, ProxyScheme};

    fn proxy(host: &str, user: &str, pass: &str) -> Proxy {
        Proxy::from_raw_parts(ProxyScheme::Http, host, "1", user, pass).expect("proxy")
    }

    #[test]
    fn a_broken_proxy_setting_builds_nothing() {
        assert_eq!(
            SignedHttp::build(ProxyState::Broken).unwrap_err(),
            AccountError::ProxyBroken
        );
    }

    #[test]
    fn the_state_a_client_reports_is_the_one_it_was_built_with() {
        let direct = SignedHttp::build(ProxyState::Direct).expect("direct");
        assert_eq!(direct.proxy_state(), &ProxyState::Direct);
        let state = ProxyState::Proxied(proxy("127.0.0.1", "", ""));
        let proxied = SignedHttp::build(state.clone()).expect("proxied");
        assert_eq!(proxied.proxy_state(), &state);
    }

    #[test]
    fn debug_shows_the_kind_of_proxy_and_never_its_credentials() {
        let state = ProxyState::Proxied(proxy("proxy.example", "alice", "hunter2"));
        let text = format!("{:?}", SignedHttp::build(state).expect("proxied"));
        assert!(text.contains("proxied"), "{text}");
        for secret in ["alice", "hunter2", "proxy.example"] {
            assert!(!text.contains(secret), "{text}");
        }
    }
}
