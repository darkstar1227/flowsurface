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
            // `no_proxy` matters: without it reqwest reads HTTPS_PROXY / ALL_PROXY from the
            // environment, so a "direct" client could leave through a proxy the user never
            // chose while the websocket (which ignores them) connects directly.
            ProxyState::Direct => builder.no_proxy(),
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

    // The environment is process-wide, so the check runs in child processes whose variables
    // are set by `Command::env` (no `set_var` in a parallel test run). `PROXY_PROBE` selects
    // the mode in the child; without it the probe tests return at once.
    const PROBE_VAR: &str = "FLOWSURFACE_PROXY_PROBE";

    /// Child side: send one request to a closed local port with the chosen client.
    fn probe(mode: &str) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _guard = rt.enter();
        let client = match mode {
            "direct" => SignedHttp::build(ProxyState::Direct)
                .expect("direct")
                .client()
                .clone(),
            // Control: a client built without `no_proxy` must be seen using the environment.
            _ => reqwest::Client::builder()
                .https_only(true)
                .build()
                .expect("plain"),
        };
        let _ = rt.block_on(client.get("https://127.0.0.1:9/").send());
    }

    #[test]
    fn direct_probe_child() {
        if std::env::var(PROBE_VAR).as_deref() == Ok("direct") {
            probe("direct");
        }
    }

    #[test]
    fn control_probe_child() {
        if std::env::var(PROBE_VAR).as_deref() == Ok("control") {
            probe("control");
        }
    }

    /// Whether the fake proxy saw a connection from a child running `test` with `mode`. The
    /// fake proxy closes every connection at once, so the child fails fast instead of waiting.
    fn proxy_was_contacted(test: &str, mode: &str) -> bool {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener");
        let addr = listener.local_addr().expect("addr");
        let url = format!("http://{addr}");
        let stop = Arc::new(AtomicBool::new(false));
        let seen = Arc::new(AtomicBool::new(false));
        let watcher = {
            let (stop, seen) = (stop.clone(), seen.clone());
            std::thread::spawn(move || {
                while let Ok(_conn) = listener.accept() {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    seen.store(true, Ordering::SeqCst);
                }
            })
        };
        let status = std::process::Command::new(std::env::current_exe().expect("exe"))
            .args(["--exact", test, "--test-threads=1"])
            .env(PROBE_VAR, mode)
            .env("HTTPS_PROXY", &url)
            .env("https_proxy", &url)
            .env("ALL_PROXY", &url)
            .env("all_proxy", &url)
            .env_remove("NO_PROXY")
            .env_remove("no_proxy")
            .status()
            .expect("child");
        // Wake the watcher with a connection that is not counted, then join it.
        stop.store(true, Ordering::SeqCst);
        let _ = std::net::TcpStream::connect(addr);
        watcher.join().expect("watcher");
        assert!(status.success(), "{test} failed in the child");
        seen.load(Ordering::SeqCst)
    }

    #[test]
    fn a_direct_client_ignores_the_proxy_environment() {
        // The control shows the probe can see a proxy being used at all.
        assert!(
            proxy_was_contacted("signed_http::tests::control_probe_child", "control"),
            "the control client did not use the environment proxy: the probe proves nothing"
        );
        assert!(
            !proxy_was_contacted("signed_http::tests::direct_probe_child", "direct"),
            "a direct SignedHttp connected through the environment proxy"
        );
    }
}
