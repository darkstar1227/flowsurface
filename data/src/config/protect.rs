//! Per-account default protective orders (M5, stage d1), saved in `saved-state.json` inside
//! [`super::exchange_accounts::AccountMeta`].
//!
//! Settings only: nothing here is a secret, no order is sent, and everything is off by default
//! (condition 78). Amounts are decimal strings (no floats), parsed by [`ProtectDefaults::spec`],
//! which refuses nonsense instead of guessing. Every field has a default, so an older save
//! without a `protect` entry loads with protection off.
//!
//! Offsets are measured from the entry **fill price**. A stop or target is either a percent of
//! it or a fixed price distance (one or the other per field). Several targets are allowed only
//! on a one-way Binance futures account: a hedge account and OKX entry attachment take one.
//! Attaching automatically needs its own switch, and a Live account needs a second one.

use exchange::account::{AccountEnv, AccountVenue, PositionMode};
use exchange::trade::{
    Decimal, MAX_TARGETS, Offset, ProtectSpec, TargetSpec, TrailingSpec, TriggerBasis,
};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

/// Binance's trailing callback rate range, in percent (docs copy; not verified live).
const MIN_CALLBACK_PCT: &str = "0.1";
const MAX_CALLBACK_PCT: &str = "10";
/// A stop below 100 % of a long's price keeps the trigger above zero.
const MAX_STOP_PCT: &str = "100";
const MAX_TARGET_PCT: &str = "1000";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ProtectOffset {
    /// Percent of the fill price, decimal string (`"1.5"` = 1.5 %).
    Percent(String),
    /// A fixed price distance, decimal string.
    Distance(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TakeProfitLeg {
    pub offset: ProtectOffset,
    /// Whole percent of the filled quantity; the legs sum to 100 and the last one takes the
    /// remainder.
    pub share_pct: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrailingDefault {
    /// Callback rate in percent, decimal string.
    pub callback_pct: String,
    /// Where the trail starts, from the fill price; `None` = at once.
    pub activation: Option<ProtectOffset>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerPriceType {
    #[default]
    Last,
    Mark,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProtectDefaults {
    pub stop_loss: Option<ProtectOffset>,
    pub take_profits: Vec<TakeProfitLeg>,
    pub trailing: Option<TrailingDefault>,
    pub trigger_price: TriggerPriceType,
    /// Send the defaults automatically after an entry fills. Off by default.
    pub auto_attach: bool,
    /// A Live account attaches automatically only when this is on too (like `auto_close`).
    pub auto_attach_live: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtectError {
    /// The named amount is not a positive decimal number, or is out of range.
    BadAmount(&'static str),
    TooManyTargets,
    /// A share is not between 1 and 100.
    BadShare,
    /// The shares do not add up to 100.
    SharesNotHundred,
    /// More than one target on a hedge account.
    MultipleTargetsNeedOneWay,
    /// More than one target on OKX (entry attachment takes one group).
    MultipleTargetsNotOnOkx,
    /// A trailing stop on a hedge or OKX account.
    TrailingNotAvailable,
    /// Something was set but the checked spec came out empty: refused, never attached as
    /// nothing.
    Dropped,
}

impl std::fmt::Display for ProtectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProtectError::BadAmount(what) => write!(f, "{what} is not a valid amount"),
            ProtectError::TooManyTargets => write!(f, "at most {MAX_TARGETS} take-profit levels"),
            ProtectError::BadShare => {
                f.write_str("each take-profit share must be 1 to 100 percent")
            }
            ProtectError::SharesNotHundred => f.write_str("take-profit shares must add up to 100"),
            ProtectError::MultipleTargetsNeedOneWay => {
                f.write_str("several take-profit levels need a one-way account")
            }
            ProtectError::MultipleTargetsNotOnOkx => f.write_str("OKX takes one take-profit level"),
            ProtectError::TrailingNotAvailable => {
                f.write_str("trailing stop is not available on this account")
            }
            ProtectError::Dropped => f.write_str("the protection settings were not kept"),
        }
    }
}

fn parse(s: &str, what: &'static str) -> Result<Decimal, ProtectError> {
    let d = Decimal::parse(s.trim()).map_err(|_| ProtectError::BadAmount(what))?;
    if d.is_positive() {
        Ok(d)
    } else {
        Err(ProtectError::BadAmount(what))
    }
}

fn within(d: Decimal, max: &str, what: &'static str) -> Result<Decimal, ProtectError> {
    let max = Decimal::parse(max).map_err(|_| ProtectError::BadAmount(what))?;
    match d.checked_cmp(&max) {
        Ok(Ordering::Less | Ordering::Equal) => Ok(d),
        _ => Err(ProtectError::BadAmount(what)),
    }
}

/// A stop at 100 % of a long's price would put the trigger at zero.
fn strictly_below(o: Offset) -> Result<Offset, ProtectError> {
    match o {
        Offset::Percent(p) if p.checked_cmp(&Decimal::from_int(100)) != Ok(Ordering::Less) => {
            Err(ProtectError::BadAmount("stop-loss"))
        }
        other => Ok(other),
    }
}

impl ProtectOffset {
    fn to_offset(&self, what: &'static str, max_pct: &str) -> Result<Offset, ProtectError> {
        match self {
            ProtectOffset::Percent(s) => {
                Ok(Offset::Percent(within(parse(s, what)?, max_pct, what)?))
            }
            ProtectOffset::Distance(s) => Ok(Offset::Distance(parse(s, what)?)),
        }
    }
}

impl ProtectDefaults {
    /// Nothing to attach.
    pub fn is_empty(&self) -> bool {
        self.stop_loss.is_none() && self.take_profits.is_empty() && self.trailing.is_none()
    }

    /// The defaults checked against the account, as the input of `exchange::trade::plan`.
    /// An invalid setting is an error, never a quiet fallback to "no protection".
    pub fn spec(
        &self,
        venue: AccountVenue,
        mode: PositionMode,
    ) -> Result<ProtectSpec, ProtectError> {
        if self.take_profits.len() > MAX_TARGETS {
            return Err(ProtectError::TooManyTargets);
        }
        if self.take_profits.len() > 1 {
            if venue == AccountVenue::Okx {
                return Err(ProtectError::MultipleTargetsNotOnOkx);
            }
            if mode == PositionMode::Hedge {
                return Err(ProtectError::MultipleTargetsNeedOneWay);
            }
        }
        let trailing = match &self.trailing {
            None => None,
            Some(t) => {
                if venue == AccountVenue::Okx || mode == PositionMode::Hedge {
                    return Err(ProtectError::TrailingNotAvailable);
                }
                let cb = parse(&t.callback_pct, "trailing callback")?;
                let lo = Decimal::parse(MIN_CALLBACK_PCT)
                    .map_err(|_| ProtectError::BadAmount("trailing callback"))?;
                if cb.checked_cmp(&lo) == Ok(Ordering::Less) {
                    return Err(ProtectError::BadAmount("trailing callback"));
                }
                within(cb, MAX_CALLBACK_PCT, "trailing callback")?;
                let activation = t
                    .activation
                    .as_ref()
                    .map(|a| a.to_offset("trailing activation", MAX_TARGET_PCT))
                    .transpose()?;
                Some(TrailingSpec {
                    callback_pct: cb,
                    activation,
                })
            }
        };
        let stop = self
            .stop_loss
            .as_ref()
            .map(|o| {
                o.to_offset("stop-loss", MAX_STOP_PCT)
                    .and_then(strictly_below)
            })
            .transpose()?;
        let mut shares = 0u32;
        let mut targets = Vec::with_capacity(self.take_profits.len());
        for leg in &self.take_profits {
            if !(1..=100).contains(&leg.share_pct) {
                return Err(ProtectError::BadShare);
            }
            shares += u32::from(leg.share_pct);
            targets.push(TargetSpec {
                offset: leg.offset.to_offset("take-profit", MAX_TARGET_PCT)?,
                share_pct: leg.share_pct,
            });
        }
        if !targets.is_empty() && shares != 100 {
            return Err(ProtectError::SharesNotHundred);
        }
        let spec = ProtectSpec {
            stop,
            targets,
            trailing,
            trigger_price: match self.trigger_price {
                TriggerPriceType::Last => TriggerBasis::Last,
                TriggerPriceType::Mark => TriggerBasis::Mark,
            },
        };
        // What was set must still be there: a trailing-only setting is not "nothing".
        if self.is_empty() != spec.is_empty() {
            return Err(ProtectError::Dropped);
        }
        Ok(spec)
    }

    /// Whether an entry fill on an account in `env` may attach the defaults by itself: the
    /// switch is on, and a Live account has its own second switch on too. Whether anything is
    /// sent at all is still decided by the send gate (Live sending is closed in this build).
    pub fn attaches_automatically(&self, env: AccountEnv) -> bool {
        self.auto_attach && (env != AccountEnv::Live || self.auto_attach_live)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pct(s: &str) -> ProtectOffset {
        ProtectOffset::Percent(s.into())
    }

    fn leg(s: &str, share: u8) -> TakeProfitLeg {
        TakeProfitLeg {
            offset: pct(s),
            share_pct: share,
        }
    }

    const ONE_WAY: PositionMode = PositionMode::OneWay;

    #[test]
    fn defaults_are_all_off() {
        let p = ProtectDefaults::default();
        assert!(p.is_empty());
        assert!(!p.auto_attach && !p.auto_attach_live);
        assert_eq!(p.trigger_price, TriggerPriceType::Last);
        assert_eq!(
            p.spec(AccountVenue::Binance, ONE_WAY),
            Ok(ProtectSpec::default())
        );
        assert!(!p.attaches_automatically(AccountEnv::Testnet));
    }

    #[test]
    fn an_empty_object_restores_the_defaults() {
        let p: ProtectDefaults = serde_json::from_str("{}").expect("empty object");
        assert_eq!(p, ProtectDefaults::default());
        // A partial object keeps the rest at their defaults.
        let p: ProtectDefaults =
            serde_json::from_str(r#"{"stop_loss":{"kind":"percent","value":"2"}}"#)
                .expect("partial");
        assert_eq!(p.stop_loss, Some(pct("2")));
        assert!(p.take_profits.is_empty() && !p.auto_attach);
    }

    #[test]
    fn round_trips_through_json() {
        let p = ProtectDefaults {
            stop_loss: Some(ProtectOffset::Distance("150.5".into())),
            take_profits: vec![leg("1", 50), leg("2", 30), leg("3", 20)],
            trailing: Some(TrailingDefault {
                callback_pct: "1.5".into(),
                activation: Some(pct("1")),
            }),
            trigger_price: TriggerPriceType::Mark,
            auto_attach: true,
            auto_attach_live: false,
        };
        let json = serde_json::to_string(&p).expect("json");
        let back: ProtectDefaults = serde_json::from_str(&json).expect("back");
        assert_eq!(back, p);
        // The shape is stable: a tagged offset and a snake_case trigger type.
        assert!(json.contains(r#""stop_loss":{"kind":"distance","value":"150.5"}"#));
        assert!(json.contains(r#""trigger_price":"mark""#));
    }

    #[test]
    fn validation_table() {
        let ok = |p: &ProtectDefaults, v, m| p.spec(v, m).is_ok();
        let with = |stop: Option<ProtectOffset>, tps: Vec<TakeProfitLeg>| ProtectDefaults {
            stop_loss: stop,
            take_profits: tps,
            ..Default::default()
        };
        let b = AccountVenue::Binance;
        // Good.
        assert!(ok(&with(Some(pct("2")), vec![leg("4", 100)]), b, ONE_WAY));
        assert!(ok(&with(Some(pct("99.9")), vec![]), b, ONE_WAY));
        assert!(ok(
            &with(None, vec![leg("1", 50), leg("2", 25), leg("3", 25)]),
            b,
            ONE_WAY
        ));
        // Non-positive, malformed and out-of-range amounts.
        for bad in [
            ProtectOffset::Percent("0".into()),
            ProtectOffset::Percent("-1".into()),
            ProtectOffset::Percent("100".into()),
            ProtectOffset::Percent("100.1".into()),
            ProtectOffset::Percent("abc".into()),
            ProtectOffset::Percent("1e2".into()),
            ProtectOffset::Distance("0".into()),
            ProtectOffset::Distance("".into()),
        ] {
            assert!(
                matches!(
                    with(Some(bad.clone()), vec![]).spec(b, ONE_WAY),
                    Err(ProtectError::BadAmount(_))
                ),
                "{bad:?}"
            );
        }
        assert!(!ok(&with(None, vec![leg("1001", 100)]), b, ONE_WAY));
        // Shares.
        assert_eq!(
            with(None, vec![leg("1", 50)]).spec(b, ONE_WAY),
            Err(ProtectError::SharesNotHundred)
        );
        assert_eq!(
            with(None, vec![leg("1", 0), leg("2", 100)]).spec(b, ONE_WAY),
            Err(ProtectError::BadShare)
        );
        assert_eq!(
            with(None, vec![leg("1", 101)]).spec(b, ONE_WAY),
            Err(ProtectError::BadShare)
        );
        assert_eq!(
            with(
                None,
                vec![leg("1", 25), leg("2", 25), leg("3", 25), leg("4", 25)]
            )
            .spec(b, ONE_WAY),
            Err(ProtectError::TooManyTargets)
        );
        // Several targets: one-way Binance only.
        let two = with(None, vec![leg("1", 50), leg("2", 50)]);
        assert_eq!(
            two.spec(b, PositionMode::Hedge),
            Err(ProtectError::MultipleTargetsNeedOneWay)
        );
        assert_eq!(
            two.spec(AccountVenue::Okx, ONE_WAY),
            Err(ProtectError::MultipleTargetsNotOnOkx)
        );
        assert!(two.spec(b, ONE_WAY).is_ok());
    }

    #[test]
    fn trailing_rules() {
        let trail = |cb: &str| ProtectDefaults {
            trailing: Some(TrailingDefault {
                callback_pct: cb.into(),
                activation: None,
            }),
            ..Default::default()
        };
        let b = AccountVenue::Binance;
        assert!(trail("1").spec(b, ONE_WAY).is_ok());
        assert!(trail("0.1").spec(b, ONE_WAY).is_ok());
        assert!(trail("10").spec(b, ONE_WAY).is_ok());
        for bad in ["0.05", "10.1", "0", "-1", "x"] {
            assert!(trail(bad).spec(b, ONE_WAY).is_err(), "{bad}");
        }
        assert_eq!(
            trail("1").spec(b, PositionMode::Hedge),
            Err(ProtectError::TrailingNotAvailable)
        );
        assert_eq!(
            trail("1").spec(AccountVenue::Okx, ONE_WAY),
            Err(ProtectError::TrailingNotAvailable)
        );
    }

    #[test]
    fn a_live_account_needs_its_own_switch_to_attach_automatically() {
        let mut p = ProtectDefaults::default();
        assert!(!p.attaches_automatically(AccountEnv::Live));
        p.auto_attach = true;
        assert!(p.attaches_automatically(AccountEnv::Testnet));
        assert!(!p.attaches_automatically(AccountEnv::Live));
        p.auto_attach_live = true;
        assert!(p.attaches_automatically(AccountEnv::Live));
        // The Live switch alone does nothing.
        p.auto_attach = false;
        assert!(!p.attaches_automatically(AccountEnv::Live));
    }

    #[test]
    fn the_spec_carries_the_parsed_values() {
        let p = ProtectDefaults {
            stop_loss: Some(pct(" 1.5 ")),
            take_profits: vec![
                TakeProfitLeg {
                    offset: ProtectOffset::Distance("10".into()),
                    share_pct: 60,
                },
                leg("5", 40),
            ],
            ..Default::default()
        };
        let spec = p.spec(AccountVenue::Binance, ONE_WAY).expect("spec");
        let d = |s: &str| Decimal::parse(s).expect("decimal");
        assert_eq!(spec.stop, Some(Offset::Percent(d("1.5"))));
        assert_eq!(spec.targets[0].offset, Offset::Distance(d("10")));
        assert_eq!(spec.targets[1].share_pct, 40);
    }

    #[test]
    fn the_spec_keeps_trailing_and_the_trigger_price() {
        let trail = |activation: Option<ProtectOffset>| ProtectDefaults {
            trailing: Some(TrailingDefault {
                callback_pct: " 1.5 ".into(),
                activation,
            }),
            trigger_price: TriggerPriceType::Mark,
            ..Default::default()
        };
        let b = AccountVenue::Binance;
        let d = |s: &str| Decimal::parse(s).expect("decimal");
        // Only a trailing stop: the spec is not empty, and says what it is.
        let spec = trail(Some(pct("2"))).spec(b, ONE_WAY).expect("spec");
        assert!(!spec.is_empty());
        assert!(spec.stop.is_none() && spec.targets.is_empty());
        let t = spec.trailing.expect("trailing");
        assert_eq!(t.callback_pct, d("1.5"));
        assert_eq!(t.activation, Some(Offset::Percent(d("2"))));
        assert_eq!(spec.trigger_price, TriggerBasis::Mark);
        // The trigger price alone is not something to attach.
        let only_basis = ProtectDefaults {
            trigger_price: TriggerPriceType::Mark,
            ..Default::default()
        };
        let spec = only_basis.spec(b, ONE_WAY).expect("spec");
        assert!(spec.is_empty() && only_basis.is_empty());
        assert_eq!(spec.trigger_price, TriggerBasis::Mark);
        // A bad activation is refused, not dropped.
        assert!(trail(Some(pct("0"))).spec(b, ONE_WAY).is_err());
    }

    /// The two `is_empty` answers agree for every combination of the three parts.
    #[test]
    fn both_is_empty_answers_agree() {
        let b = AccountVenue::Binance;
        for stop in [false, true] {
            for tp in [false, true] {
                for trail in [false, true] {
                    let p = ProtectDefaults {
                        stop_loss: stop.then(|| pct("1")),
                        take_profits: if tp { vec![leg("2", 100)] } else { vec![] },
                        trailing: trail.then(|| TrailingDefault {
                            callback_pct: "1".into(),
                            activation: None,
                        }),
                        ..Default::default()
                    };
                    let spec = p.spec(b, ONE_WAY).expect("spec");
                    assert_eq!(p.is_empty(), spec.is_empty(), "{stop} {tp} {trail}");
                }
            }
        }
    }
}
