//! Per-account local risk settings (segment e), saved in `saved-state.json` inside
//! [`super::exchange_accounts::AccountMeta`].
//!
//! Only settings live here: no equity, no peak, no lock state (those are in-memory
//! `exchange::trade::EquityTracker` state and never written, condition 73 and 78). Nothing
//! here is a secret. Amounts are decimal strings (no floats), parsed by
//! [`AccountRiskSettings::limits`], which refuses nonsense instead of guessing.
//!
//! Every field has a default, so an older save without a `risk` entry loads, and automatic
//! close defaults to off (user decision 2).

use exchange::account::AccountKey;
use exchange::trade::{Decimal, DrawdownAnchor, EquityBasis, EquityTracker, RiskLimits};
use serde::{Deserialize, Serialize};

const MAX_BPS: u32 = 10_000;
const MIN_EQUITY_AGE_SECS: u32 = 10;
const MAX_EQUITY_AGE_SECS: u32 = 3_600;
const MAX_CONFIRM_OBSERVATIONS: u32 = 100;
/// The furthest UTC offset in use is +14:00 (and -12:00 at the other end).
const MAX_OFFSET_MINUTES: i32 = 14 * 60;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AccountRiskSettings {
    /// Largest value of one new order, quote currency, decimal string. `None` = no cap.
    pub max_order_notional: Option<String>,
    /// Largest position value per symbol after the order, quote currency.
    pub max_symbol_notional: Option<String>,
    /// Daily loss limit, quote currency.
    pub daily_loss_limit: Option<String>,
    /// Drawdown from the peak, in basis points (1000 = 10%).
    pub drawdown_bps: Option<u32>,
    /// Custom start (Unix ms) for the drawdown peak. `None` = since the start of the day.
    pub drawdown_start_ms: Option<i64>,
    /// Consecutive readings beyond a limit before it trips.
    pub confirm_observations: u32,
    /// Equity readings older than this many seconds are stale.
    pub max_equity_age_secs: u32,
    /// Day boundary as an offset from UTC, in minutes (the time zone the "day" is in).
    pub day_utc_offset_minutes: i32,
    /// Cancel everything and close every position when a limit trips. Off by default; a Live
    /// account must be switched on explicitly.
    pub auto_close: bool,
}

impl Default for AccountRiskSettings {
    fn default() -> Self {
        let l = RiskLimits::default();
        Self {
            max_order_notional: None,
            max_symbol_notional: None,
            daily_loss_limit: None,
            drawdown_bps: None,
            drawdown_start_ms: None,
            confirm_observations: l.confirm_observations,
            max_equity_age_secs: 120,
            day_utc_offset_minutes: 0,
            auto_close: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiskSettingsError {
    /// An amount is not a positive decimal number.
    BadAmount(&'static str),
    BadDrawdown,
    BadConfirmCount,
    BadEquityAge,
    BadDayOffset,
}

impl AccountRiskSettings {
    /// The limits the risk tracker runs with. An invalid setting is an error, not a default:
    /// the caller must not fall back to "no limits" silently.
    pub fn limits(&self) -> Result<RiskLimits, RiskSettingsError> {
        let amount = |name: &'static str, v: &Option<String>| match v {
            None => Ok(None),
            Some(s) => match Decimal::parse(s.trim()) {
                Ok(d) if d.is_positive() => Ok(Some(d)),
                _ => Err(RiskSettingsError::BadAmount(name)),
            },
        };
        if self.drawdown_bps.is_some_and(|b| b == 0 || b > MAX_BPS) {
            return Err(RiskSettingsError::BadDrawdown);
        }
        if self.confirm_observations == 0 || self.confirm_observations > MAX_CONFIRM_OBSERVATIONS {
            return Err(RiskSettingsError::BadConfirmCount);
        }
        if !(MIN_EQUITY_AGE_SECS..=MAX_EQUITY_AGE_SECS).contains(&self.max_equity_age_secs) {
            return Err(RiskSettingsError::BadEquityAge);
        }
        if self.day_utc_offset_minutes.abs() > MAX_OFFSET_MINUTES {
            return Err(RiskSettingsError::BadDayOffset);
        }
        Ok(RiskLimits {
            max_order_notional: amount("max_order_notional", &self.max_order_notional)?,
            max_symbol_notional: amount("max_symbol_notional", &self.max_symbol_notional)?,
            daily_loss_limit: amount("daily_loss_limit", &self.daily_loss_limit)?,
            drawdown_bps: self.drawdown_bps,
            anchor: match self.drawdown_start_ms {
                Some(ms) => DrawdownAnchor::Since(ms),
                None => DrawdownAnchor::Day,
            },
            confirm_observations: self.confirm_observations,
            max_equity_age_ms: i64::from(self.max_equity_age_secs) * 1000,
            day_utc_offset_secs: self.day_utc_offset_minutes * 60,
        })
    }
}

impl AccountRiskSettings {
    /// The tracker for this account. Invalid settings give a tracker that refuses every new
    /// entry (reducing still passes), never one without limits: the tracker is the only
    /// source of the risk lock, so the gate and the UI read the same answer.
    pub fn tracker(&self, account: AccountKey, basis: EquityBasis) -> EquityTracker {
        match self.limits() {
            Ok(limits) => EquityTracker::new(account, basis, limits),
            Err(e) => {
                log::warn!("risk settings invalid, new entries blocked: {e:?}");
                EquityTracker::with_invalid_settings(account, basis)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_settings_block_entries_but_not_closes() {
        use exchange::trade::{Exposure, RiskHook, RiskRefusal, RiskView};
        let account = AccountKey::from_bytes([3; 16]);
        let bad = AccountRiskSettings {
            max_order_notional: Some("-1".into()),
            ..AccountRiskSettings::default()
        };
        assert!(bad.limits().is_err());
        let t = bad.tracker(account, EquityBasis::OkxTotalEq);
        assert!(t.settings_invalid());
        let snap = t.snapshot(0);
        let entry = RiskView {
            account,
            symbol: "BTCUSDT",
            exposure: Exposure::Adds { notional: None },
        };
        assert_eq!(snap.check(&entry), Err(RiskRefusal::SettingsInvalid));
        let close = RiskView {
            exposure: Exposure::Reduces,
            ..entry
        };
        assert_eq!(snap.check(&close), Ok(()));
        // Valid settings give a normal tracker.
        let ok = AccountRiskSettings::default().tracker(account, EquityBasis::OkxTotalEq);
        assert!(!ok.settings_invalid());
    }

    #[test]
    fn defaults_have_no_limits_and_automatic_close_off() {
        let s = AccountRiskSettings::default();
        assert!(!s.auto_close);
        let l = s.limits().expect("defaults are valid");
        assert_eq!(l, RiskLimits::default());
    }

    #[test]
    fn an_old_save_without_risk_fields_loads_with_defaults() {
        let s: AccountRiskSettings = serde_json::from_str("{}").expect("empty object");
        assert_eq!(s, AccountRiskSettings::default());
        // A partial object keeps the rest at their defaults.
        let s: AccountRiskSettings =
            serde_json::from_str(r#"{"daily_loss_limit":"250.5"}"#).expect("partial");
        assert_eq!(s.daily_loss_limit.as_deref(), Some("250.5"));
        assert!(!s.auto_close);
        assert_eq!(s.confirm_observations, 3);
    }

    #[test]
    fn settings_convert_to_limits() {
        let s = AccountRiskSettings {
            max_order_notional: Some("1000".into()),
            max_symbol_notional: Some(" 5000.25 ".into()),
            daily_loss_limit: Some("200".into()),
            drawdown_bps: Some(800),
            drawdown_start_ms: Some(1_700_000_000_000),
            confirm_observations: 5,
            max_equity_age_secs: 60,
            day_utc_offset_minutes: 8 * 60,
            auto_close: true,
        };
        let l = s.limits().expect("valid");
        assert_eq!(l.max_order_notional, Decimal::parse("1000").ok());
        assert_eq!(l.max_symbol_notional, Decimal::parse("5000.25").ok());
        assert_eq!(l.drawdown_bps, Some(800));
        assert_eq!(l.anchor, DrawdownAnchor::Since(1_700_000_000_000));
        assert_eq!(l.confirm_observations, 5);
        assert_eq!(l.max_equity_age_ms, 60_000);
        assert_eq!(l.day_utc_offset_secs, 8 * 3600);
    }

    #[test]
    fn nonsense_is_an_error_not_a_default() {
        let bad = |f: &dyn Fn(&mut AccountRiskSettings)| {
            let mut s = AccountRiskSettings::default();
            f(&mut s);
            s.limits()
        };
        for text in ["", "abc", "-5", "0", "1e3", "NaN"] {
            assert_eq!(
                bad(&|s| s.max_order_notional = Some(text.into())),
                Err(RiskSettingsError::BadAmount("max_order_notional")),
                "{text:?}"
            );
        }
        assert_eq!(
            bad(&|s| s.drawdown_bps = Some(0)),
            Err(RiskSettingsError::BadDrawdown)
        );
        assert_eq!(
            bad(&|s| s.drawdown_bps = Some(10_001)),
            Err(RiskSettingsError::BadDrawdown)
        );
        assert_eq!(
            bad(&|s| s.confirm_observations = 0),
            Err(RiskSettingsError::BadConfirmCount)
        );
        assert_eq!(
            bad(&|s| s.max_equity_age_secs = 1),
            Err(RiskSettingsError::BadEquityAge)
        );
        assert_eq!(
            bad(&|s| s.day_utc_offset_minutes = 15 * 60),
            Err(RiskSettingsError::BadDayOffset)
        );
    }
}
