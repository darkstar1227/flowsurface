//! Local risk controls (segment e, conditions 71–73).
//!
//! Everything here is pure: no clock, no network, no files. Time arrives as arguments
//! (`at_ms` of an observation, `now_ms` of a check), so tests never read the wall clock.
//!
//! - [`EquityTracker`] takes [`EquityObservation`]s (from a REST read now, from the private
//!   stream later) and keeps the day's starting equity and the peak, in memory only
//!   (condition 73: a restart resets them). It trips on the daily loss limit or the drawdown
//!   limit, but only after `confirm_observations` consecutive observations beyond the
//!   threshold, and a flagged transfer re-bases the references instead of counting.
//! - [`RiskSnapshot`] is the tracker as seen at one instant. It implements [`RiskHook`] and
//!   is what the gate calls: when the tracker is tripped, or its equity is unknown or stale,
//!   or a cap is exceeded or cannot be judged, new entries are refused. Reducing, closing and
//!   cancelling are never refused here (condition 72).
//! - [`EquityTracker::begin_auto_close`] decides whether an automatic cancel-all and flatten
//!   may start. It is off unless the account enabled it, never starts on stale equity, and
//!   runs at most once per trip. It returns an [`AutoCloseGrant`], which `TradeClient::auto_close`
//!   requires (user decision 2, condition 71).
//!
//! ```compile_fail,E0451
//! use flowsurface_exchange::trade::AutoCloseGrant;
//! use flowsurface_exchange::account::AccountKey;
//! // The grant cannot be built outside the crate.
//! fn forge(account: AccountKey) -> AutoCloseGrant { AutoCloseGrant { account } }
//! ```
//!
//! Equity fields by venue (condition 73) are NOT verified against a testnet response yet:
//! Binance USDⓈ-M `totalMarginBalance` (wallet balance plus unrealised PnL), OKX `totalEq`,
//! spot valued at mark prices. [`EquityBasis`] only labels what the caller says it fed in; the
//! tracker refuses observations of another basis.

use super::decimal::Decimal;
use super::gate::{RiskHook, RiskRefusal, RiskView};
use super::request::Exposure;
use crate::account::AccountKey;
use std::cmp::Ordering;
use std::collections::HashMap;

/// Which figure an equity observation is (shown in the UI, condition 73).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EquityBasis {
    /// Binance USDⓈ-M `totalMarginBalance`. Field not yet checked on a testnet response.
    BinanceTotalMarginBalance,
    /// OKX `account/balance` `totalEq`. Field not yet checked on a demo response.
    OkxTotalEq,
    /// Binance spot balances valued at mark prices.
    SpotMarkValued,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EquitySource {
    Rest,
    /// The private stream (segment b/c wiring).
    PrivateStream,
}

/// One reading of the account's equity, in the quote currency of the account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EquityObservation {
    pub at_ms: i64,
    pub equity: Decimal,
    pub basis: EquityBasis,
    pub source: EquitySource,
    /// The change since the previous observation came from a deposit or withdrawal
    /// (`ACCOUNT_UPDATE` reason, or an OKX funding bill). It re-bases the day start and the
    /// peak and is not counted as a loss or a recovery.
    pub is_transfer: bool,
}

/// Where the drawdown peak is taken from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrawdownAnchor {
    /// The highest equity since the start of the current day (in the configured offset).
    Day,
    /// The highest equity since a custom start (Unix ms).
    Since(i64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RiskLimits {
    /// Largest value of one new order, in the quote currency. An entry whose value cannot be
    /// computed is refused while this (or `max_symbol_notional`) is set (review item 16).
    pub max_order_notional: Option<Decimal>,
    /// Largest position value per symbol after the order, in the quote currency.
    pub max_symbol_notional: Option<Decimal>,
    /// Daily loss limit, an amount in the quote currency.
    pub daily_loss_limit: Option<Decimal>,
    /// Drawdown from the peak, in basis points (1000 = 10%).
    pub drawdown_bps: Option<u32>,
    pub anchor: DrawdownAnchor,
    /// Consecutive observations beyond a threshold before it trips (at least 1).
    pub confirm_observations: u32,
    /// An equity reading older than this is stale.
    pub max_equity_age_ms: i64,
    /// Offset of the day boundary from UTC, in seconds (the user's time zone).
    pub day_utc_offset_secs: i32,
}

impl Default for RiskLimits {
    fn default() -> Self {
        Self {
            max_order_notional: None,
            max_symbol_notional: None,
            daily_loss_limit: None,
            drawdown_bps: None,
            anchor: DrawdownAnchor::Day,
            confirm_observations: 3,
            max_equity_age_ms: 120_000,
            day_utc_offset_secs: 0,
        }
    }
}

impl RiskLimits {
    fn equity_limits_set(&self) -> bool {
        self.daily_loss_limit.is_some() || self.drawdown_bps.is_some()
    }
}

/// The local calendar day of a Unix millisecond timestamp, as a day number, for a day boundary
/// `offset_secs` east of UTC.
pub fn local_day(ts_ms: i64, offset_secs: i32) -> i64 {
    (ts_ms.div_euclid(1000) + i64::from(offset_secs)).div_euclid(86_400)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TripReason {
    DailyLoss,
    Drawdown,
}

impl TripReason {
    pub fn as_str(self) -> &'static str {
        match self {
            TripReason::DailyLoss => "daily_loss",
            TripReason::Drawdown => "drawdown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Trip {
    reason: TripReason,
    at_ms: i64,
    /// A day-scoped trip clears when the next day starts.
    day_scoped: bool,
    /// The automatic close was decided for this trip (run, or skipped): it is never started
    /// again for the same trip, so enabling it later does not flatten retroactively.
    auto_handled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IgnoreReason {
    /// The observation is of another basis than the tracker's.
    BasisMismatch,
    /// Zero or negative equity: treated as bad data, never as a loss.
    NotPositive,
    /// Not newer than the last accepted observation.
    OutOfOrder,
    /// The arithmetic did not fit; the equity is marked unavailable.
    Arithmetic,
}

/// What one observation did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Observed {
    Ignored(IgnoreReason),
    Accepted,
    /// This observation completed the confirmation streak.
    Tripped(RiskNotice),
}

/// An event for the GUI (shown by segment c) and for the risk log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RiskNotice {
    pub account: AccountKey,
    pub at_ms: i64,
    pub kind: RiskNoticeKind,
}

impl RiskNotice {
    /// The close was decided against because automatic close is off for the account.
    pub fn auto_close_skipped(account: AccountKey, at_ms: i64, reason: TripReason) -> Self {
        Self {
            account,
            at_ms,
            kind: RiskNoticeKind::AutoCloseSkippedDisabled { reason },
        }
    }

    pub fn auto_close_started(grant: &AutoCloseGrant, at_ms: i64) -> Self {
        Self {
            account: grant.account,
            at_ms,
            kind: RiskNoticeKind::AutoCloseStarted {
                reason: grant.reason,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiskNoticeKind {
    /// A limit tripped: new entries are refused from now on.
    Tripped {
        reason: TripReason,
        equity: Decimal,
        /// The day's starting equity (daily loss) or the peak (drawdown).
        reference: Decimal,
    },
    /// The trip would have closed everything, but automatic close is off for this account.
    AutoCloseSkippedDisabled { reason: TripReason },
    /// Automatic cancel-all and flatten started.
    AutoCloseStarted { reason: TripReason },
    /// Automatic close finished and the account was read again.
    AutoCloseFinished {
        clear: bool,
        /// `None`: that final read failed, so the count is not known (not zero).
        remaining_orders: Option<usize>,
        remaining_positions: Option<usize>,
        /// A final read failed, so the counts are not known.
        reread_failed: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EquityFreshness {
    /// Never observed.
    Unknown,
    /// Too old, from the future, or the last reading was unusable.
    Stale,
    Fresh,
}

/// Why an automatic close did or did not start.
#[derive(Debug, PartialEq, Eq)]
pub enum AutoCloseDecision {
    NotTripped,
    /// Automatic close is off for this account (the default).
    Disabled,
    /// The equity is unknown or stale: no automatic close (condition 73).
    EquityUnavailable,
    /// The equity source cannot flag transfers (REST reads), so a withdrawal would look like
    /// a loss: no automatic close. Decided for this trip, like `Disabled`.
    TransferBlind,
    /// Already started (or skipped) for this trip.
    AlreadyHandled,
    Run(AutoCloseGrant),
}

/// Proof that [`EquityTracker::begin_auto_close`] said yes. Not `Clone`, no public
/// constructor: `TradeClient::auto_close` needs one, so only a tripped, fresh, enabled tracker
/// can start an automatic close.
#[derive(Debug, PartialEq, Eq)]
pub struct AutoCloseGrant {
    pub(crate) account: AccountKey,
    reason: TripReason,
}

impl AutoCloseGrant {
    pub fn reason(&self) -> TripReason {
        self.reason
    }

    pub fn account(&self) -> AccountKey {
        self.account
    }
}

#[derive(Debug, Clone)]
struct PositionBook {
    as_of_ms: i64,
    notionals: HashMap<String, Decimal>,
}

/// Per-account risk state, in memory only (the peak is never saved, condition 73).
///
/// Not `Clone`: the once-per-trip mark lives here, and a copy would hand out a second
/// [`AutoCloseGrant`] for the same trip.
///
/// ```compile_fail,E0599
/// use flowsurface_exchange::trade::EquityTracker;
/// // `Clone` is missing on purpose; this fails only because of that.
/// fn copy(t: &EquityTracker) -> EquityTracker { t.clone() }
/// ```
#[derive(Debug)]
pub struct EquityTracker {
    account: AccountKey,
    basis: EquityBasis,
    limits: RiskLimits,
    last: Option<(i64, Decimal)>,
    day: Option<i64>,
    day_start: Option<Decimal>,
    peak: Option<Decimal>,
    loss_streak: u32,
    drawdown_streak: u32,
    trip: Option<Trip>,
    /// Local day on which the user lifted a daily-loss trip. A daily-loss trip raised again
    /// on that same day is already handled: no second automatic close (review E3).
    loss_trip_lifted_day: Option<i64>,
    /// The latest accepted reading came from a source that flags transfers.
    transfer_aware: bool,
    /// The latest reading could not be used; equity counts as stale until a good one arrives.
    degraded: bool,
    positions: Option<PositionBook>,
}

impl EquityTracker {
    pub fn new(account: AccountKey, basis: EquityBasis, limits: RiskLimits) -> Self {
        Self {
            account,
            basis,
            limits,
            last: None,
            day: None,
            day_start: None,
            peak: None,
            loss_streak: 0,
            drawdown_streak: 0,
            trip: None,
            loss_trip_lifted_day: None,
            transfer_aware: false,
            degraded: false,
            positions: None,
        }
    }

    pub fn limits(&self) -> &RiskLimits {
        &self.limits
    }

    pub fn basis(&self) -> EquityBasis {
        self.basis
    }

    /// Change the limits (settings edited). Streaks restart, and the peak restarts when the
    /// anchor changes. A trip stays until it clears by
    /// itself or [`EquityTracker::acknowledge_trip`].
    pub fn set_limits(&mut self, limits: RiskLimits) {
        // A different anchor means a different peak: start again from the latest reading,
        // or a new custom start would still be measured from an earlier high.
        if limits.anchor != self.limits.anchor {
            self.peak = match (limits.anchor, self.last) {
                (DrawdownAnchor::Since(start), Some((at, eq))) if at >= start => Some(eq),
                (DrawdownAnchor::Day, Some((_, eq))) => Some(eq),
                _ => None,
            };
        }
        self.limits = limits;
        self.loss_streak = 0;
        self.drawdown_streak = 0;
    }

    pub fn is_tripped(&self) -> Option<TripReason> {
        self.trip.map(|t| t.reason)
    }

    /// The user acknowledged a trip and lifts the lock (a custom-start drawdown trip never
    /// clears by itself). The peak is reset to the current equity so it does not trip again at
    /// once.
    pub fn acknowledge_trip(&mut self) {
        if let Some(t) = self.trip
            && t.reason == TripReason::DailyLoss
        {
            self.loss_trip_lifted_day = self.day;
        }
        self.trip = None;
        self.loss_streak = 0;
        self.drawdown_streak = 0;
        if let Some((_, equity)) = self.last {
            self.peak = Some(equity);
        }
    }

    /// Record the position values (quote currency, absolute, long and short of a symbol
    /// summed) read from the exchange at `as_of_ms`, for the per-symbol cap.
    pub fn set_positions(
        &mut self,
        as_of_ms: i64,
        notionals: impl IntoIterator<Item = (String, Decimal)>,
    ) {
        self.positions = Some(PositionBook {
            as_of_ms,
            notionals: notionals.into_iter().collect(),
        });
    }

    pub fn freshness(&self, now_ms: i64) -> EquityFreshness {
        let Some((at_ms, _)) = self.last else {
            return EquityFreshness::Unknown;
        };
        // A reading dated well ahead of now is not trusted either (clock skew).
        let age = now_ms.saturating_sub(at_ms);
        if self.degraded || age > self.limits.max_equity_age_ms || age < -MAX_FUTURE_SKEW_MS {
            EquityFreshness::Stale
        } else {
            EquityFreshness::Fresh
        }
    }

    /// Feed one equity reading. Returns [`Observed::Tripped`] only on the observation that
    /// completes the streak.
    pub fn observe(&mut self, obs: EquityObservation) -> Observed {
        if obs.basis != self.basis {
            return Observed::Ignored(IgnoreReason::BasisMismatch);
        }
        if !obs.equity.is_positive() {
            self.degraded = true;
            return Observed::Ignored(IgnoreReason::NotPositive);
        }
        if let Some((last_at, _)) = self.last
            && obs.at_ms <= last_at
        {
            return Observed::Ignored(IgnoreReason::OutOfOrder);
        }
        match self.apply(obs) {
            Ok(observed) => {
                self.degraded = false;
                self.transfer_aware = obs.source == EquitySource::PrivateStream;
                observed
            }
            Err(_) => {
                self.degraded = true;
                self.loss_streak = 0;
                self.drawdown_streak = 0;
                Observed::Ignored(IgnoreReason::Arithmetic)
            }
        }
    }

    fn apply(&mut self, obs: EquityObservation) -> Result<Observed, super::decimal::DecimalError> {
        let day = local_day(obs.at_ms, self.limits.day_utc_offset_secs);
        let previous = self.last;
        self.last = Some((obs.at_ms, obs.equity));

        let new_day = self.day != Some(day);
        if new_day {
            self.day = Some(day);
            self.day_start = Some(obs.equity);
            if self.limits.anchor == DrawdownAnchor::Day {
                self.peak = Some(obs.equity);
            }
            self.loss_streak = 0;
            self.drawdown_streak = 0;
            self.loss_trip_lifted_day = None;
            if self.trip.is_some_and(|t| t.day_scoped) {
                self.trip = None;
            }
        } else if obs.is_transfer {
            // Re-base by the equity change since the last reading, so a deposit is not a
            // recovery and a withdrawal is not a loss.
            if let Some((_, before)) = previous {
                let delta = obs.equity.checked_sub(&before)?;
                self.day_start = self.rebase(self.day_start, &delta, obs.equity)?;
                self.peak = self.rebase(self.peak, &delta, obs.equity)?;
            }
            self.loss_streak = 0;
            self.drawdown_streak = 0;
            return Ok(Observed::Accepted);
        }

        let track_peak = match self.limits.anchor {
            DrawdownAnchor::Day => true,
            DrawdownAnchor::Since(start) => obs.at_ms >= start,
        };
        if track_peak {
            self.peak = Some(match self.peak {
                Some(p) if p.checked_cmp(&obs.equity)? != Ordering::Less => p,
                _ => obs.equity,
            });
        }

        let loss_breach = match (self.limits.daily_loss_limit, self.day_start) {
            (Some(limit), Some(start)) => {
                limit.is_positive()
                    && start.checked_sub(&obs.equity)?.checked_cmp(&limit)? != Ordering::Less
            }
            _ => false,
        };
        let drawdown_breach = match (self.limits.drawdown_bps, self.peak) {
            (Some(bps), Some(peak)) if bps > 0 => {
                let drop = peak
                    .checked_sub(&obs.equity)?
                    .checked_mul(&Decimal::from_int(10_000))?;
                let allowed = peak.checked_mul(&Decimal::from_int(i64::from(bps)))?;
                drop.is_positive() && drop.checked_cmp(&allowed)? != Ordering::Less
            }
            _ => false,
        };
        self.loss_streak = if loss_breach { self.loss_streak + 1 } else { 0 };
        self.drawdown_streak = if drawdown_breach {
            self.drawdown_streak + 1
        } else {
            0
        };

        let need = self.limits.confirm_observations.max(1);
        let reason = if self.loss_streak >= need {
            Some((TripReason::DailyLoss, self.day_start, true))
        } else if self.drawdown_streak >= need {
            Some((
                TripReason::Drawdown,
                self.peak,
                self.limits.anchor == DrawdownAnchor::Day,
            ))
        } else {
            None
        };
        if let (Some((reason, reference, day_scoped)), None) = (reason, self.trip) {
            self.trip = Some(Trip {
                reason,
                at_ms: obs.at_ms,
                day_scoped,
                // Lifted earlier today: the account was already handled for this loss.
                auto_handled: reason == TripReason::DailyLoss
                    && self.loss_trip_lifted_day == self.day,
            });
            return Ok(Observed::Tripped(RiskNotice {
                account: self.account,
                at_ms: obs.at_ms,
                kind: RiskNoticeKind::Tripped {
                    reason,
                    equity: obs.equity,
                    reference: reference.unwrap_or(obs.equity),
                },
            }));
        }
        Ok(Observed::Accepted)
    }

    /// `reference + delta`, or the current equity when that would not be positive.
    fn rebase(
        &self,
        reference: Option<Decimal>,
        delta: &Decimal,
        equity: Decimal,
    ) -> Result<Option<Decimal>, super::decimal::DecimalError> {
        let Some(r) = reference else {
            return Ok(None);
        };
        let moved = r.checked_add(delta)?;
        Ok(Some(if moved.is_positive() { moved } else { equity }))
    }

    /// The tracker as the gate sees it at `now_ms`.
    pub fn snapshot(&self, now_ms: i64) -> RiskSnapshot<'_> {
        RiskSnapshot {
            tracker: self,
            now_ms,
        }
    }

    /// Decide whether an automatic cancel-all and flatten may start now. `enabled` is the
    /// account's own setting (default off; a Live account must have turned it on).
    pub fn begin_auto_close(&mut self, now_ms: i64, enabled: bool) -> AutoCloseDecision {
        let Some(trip) = self.trip else {
            return AutoCloseDecision::NotTripped;
        };
        if trip.auto_handled {
            return AutoCloseDecision::AlreadyHandled;
        }
        if !enabled {
            // Decided for this trip: turning the setting on later does not flatten
            // retroactively.
            self.mark_handled();
            return AutoCloseDecision::Disabled;
        }
        // A REST reading cannot tell a withdrawal from a loss: never flatten on it. Only a
        // source that flags transfers (the private stream) may start an automatic close.
        if !self.transfer_aware {
            self.mark_handled();
            return AutoCloseDecision::TransferBlind;
        }
        // Stale or unknown equity never starts an automatic close (condition 73).
        if self.freshness(now_ms) != EquityFreshness::Fresh {
            return AutoCloseDecision::EquityUnavailable;
        }
        self.mark_handled();
        AutoCloseDecision::Run(AutoCloseGrant {
            account: self.account,
            reason: trip.reason,
        })
    }

    fn mark_handled(&mut self) {
        if let Some(t) = &mut self.trip {
            t.auto_handled = true;
        }
    }
}

/// A reading dated more than this ahead of `now` is not trusted.
const MAX_FUTURE_SKEW_MS: i64 = 5_000;

/// The tracker at one instant: the [`RiskHook`] the gate calls.
#[derive(Debug, Clone, Copy)]
pub struct RiskSnapshot<'a> {
    tracker: &'a EquityTracker,
    now_ms: i64,
}

impl RiskHook for RiskSnapshot<'_> {
    fn check(&self, view: &RiskView<'_>) -> Result<(), RiskRefusal> {
        let t = self.tracker;
        if view.account != t.account {
            return Err(RiskRefusal::WrongAccount);
        }
        // Only entries are judged: reducing, closing and cancelling always pass (condition 72).
        let Exposure::Adds { notional } = view.exposure else {
            return Ok(());
        };
        if t.trip.is_some() {
            return Err(RiskRefusal::Locked);
        }
        if t.limits.equity_limits_set() && t.freshness(self.now_ms) != EquityFreshness::Fresh {
            return Err(RiskRefusal::EquityUnavailable);
        }
        // An entry that cannot be valued is refused whether or not a cap is set (review item
        // 16, as in the single-order cap this replaces): the app never sends an entry it
        // cannot put a value on.
        let Some(value) = notional else {
            return Err(RiskRefusal::NotionalUnknown);
        };
        if let Some(cap) = t.limits.max_order_notional
            && !matches!(
                value.checked_cmp(&cap),
                Ok(Ordering::Less | Ordering::Equal)
            )
        {
            return Err(RiskRefusal::SingleOrderCapExceeded);
        }
        if let Some(cap) = t.limits.max_symbol_notional {
            let Some(book) = &t.positions else {
                return Err(RiskRefusal::PositionUnknown);
            };
            let age = self.now_ms.saturating_sub(book.as_of_ms);
            if age > t.limits.max_equity_age_ms || age < -MAX_FUTURE_SKEW_MS {
                return Err(RiskRefusal::PositionUnknown);
            }
            let held = book
                .notionals
                .get(view.symbol)
                .copied()
                .unwrap_or(Decimal::ZERO);
            let after = held
                .checked_add(&value)
                .map_err(|_| RiskRefusal::SymbolCapExceeded)?;
            if !matches!(
                after.checked_cmp(&cap),
                Ok(Ordering::Less | Ordering::Equal)
            ) {
                return Err(RiskRefusal::SymbolCapExceeded);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACCT: AccountKey = AccountKey::from_bytes([7; 16]);
    const MIN: i64 = 60_000;
    const DAY: i64 = 86_400_000;
    // 2024-01-02 00:00:00 UTC
    const T0: i64 = 1_704_153_600_000;

    fn d(s: &str) -> Decimal {
        Decimal::parse(s).expect("decimal")
    }

    fn obs(at_ms: i64, equity: &str) -> EquityObservation {
        EquityObservation {
            at_ms,
            equity: d(equity),
            basis: EquityBasis::OkxTotalEq,
            source: EquitySource::Rest,
            is_transfer: false,
        }
    }

    /// A reading from the private stream, the only source that can flag transfers.
    fn stream(at_ms: i64, equity: &str) -> EquityObservation {
        EquityObservation {
            source: EquitySource::PrivateStream,
            ..obs(at_ms, equity)
        }
    }

    fn transfer(at_ms: i64, equity: &str) -> EquityObservation {
        EquityObservation {
            is_transfer: true,
            ..obs(at_ms, equity)
        }
    }

    fn limits() -> RiskLimits {
        RiskLimits {
            daily_loss_limit: Some(d("100")),
            drawdown_bps: Some(1000),
            confirm_observations: 3,
            ..RiskLimits::default()
        }
    }

    fn tracker(l: RiskLimits) -> EquityTracker {
        EquityTracker::new(ACCT, EquityBasis::OkxTotalEq, l)
    }

    fn entry(notional: Option<&str>) -> Exposure {
        Exposure::Adds {
            notional: notional.map(d),
        }
    }

    fn verdict(t: &EquityTracker, now: i64, exposure: Exposure) -> Result<(), RiskRefusal> {
        t.snapshot(now).check(&RiskView {
            account: ACCT,
            symbol: "BTC-USDT-SWAP",
            exposure,
        })
    }

    fn tripped(o: Observed) -> bool {
        matches!(o, Observed::Tripped(_))
    }

    #[test]
    fn local_day_follows_the_offset() {
        // 23:30 UTC on day N is already day N+1 at UTC+8, and still day N at UTC-5.
        let late = T0 + 23 * 3_600_000 + 30 * MIN;
        let utc = local_day(late, 0);
        assert_eq!(local_day(late, 8 * 3600), utc + 1);
        assert_eq!(local_day(late, -5 * 3600), utc);
        assert_eq!(local_day(T0, -5 * 3600), utc - 1);
        // Before the epoch rounds down, not toward zero.
        assert_eq!(local_day(-1, 0), -1);
    }

    #[test]
    fn nothing_observed_blocks_entries_but_not_reducing() {
        let t = tracker(limits());
        assert_eq!(t.freshness(T0), EquityFreshness::Unknown);
        assert_eq!(
            verdict(&t, T0, entry(Some("10"))),
            Err(RiskRefusal::EquityUnavailable)
        );
        assert_eq!(verdict(&t, T0, Exposure::Reduces), Ok(()));
        assert_eq!(verdict(&t, T0, Exposure::Neutral), Ok(()));
    }

    #[test]
    fn stale_equity_blocks_entries_and_never_trips_or_closes() {
        let mut t = tracker(limits());
        assert_eq!(t.observe(obs(T0, "1000")), Observed::Accepted);
        assert_eq!(verdict(&t, T0 + MIN, entry(Some("10"))), Ok(()));
        // Older than max_equity_age_ms.
        let late = T0 + 10 * MIN;
        assert_eq!(t.freshness(late), EquityFreshness::Stale);
        assert_eq!(
            verdict(&t, late, entry(Some("10"))),
            Err(RiskRefusal::EquityUnavailable)
        );
        assert_eq!(verdict(&t, late, Exposure::Reduces), Ok(()));
        // Not tripped, so no automatic close, whatever the setting.
        assert_eq!(
            t.begin_auto_close(late, true),
            AutoCloseDecision::NotTripped
        );
    }

    #[test]
    fn a_reading_from_the_future_is_stale() {
        let mut t = tracker(limits());
        t.observe(obs(T0 + 10 * MIN, "1000"));
        assert_eq!(t.freshness(T0), EquityFreshness::Stale);
    }

    #[test]
    fn bad_readings_are_ignored_and_make_equity_stale() {
        let mut t = tracker(limits());
        t.observe(obs(T0, "1000"));
        assert_eq!(
            t.observe(obs(T0 + MIN, "0")),
            Observed::Ignored(IgnoreReason::NotPositive)
        );
        assert_eq!(t.freshness(T0 + MIN), EquityFreshness::Stale);
        assert_eq!(
            t.observe(obs(T0 + MIN, "-5")),
            Observed::Ignored(IgnoreReason::NotPositive)
        );
        let mut wrong = obs(T0 + 2 * MIN, "900");
        wrong.basis = EquityBasis::BinanceTotalMarginBalance;
        assert_eq!(
            t.observe(wrong),
            Observed::Ignored(IgnoreReason::BasisMismatch)
        );
        // An older or equal timestamp cannot rewrite history.
        assert_eq!(
            t.observe(obs(T0, "500")),
            Observed::Ignored(IgnoreReason::OutOfOrder)
        );
        // A good reading clears the degraded mark.
        assert_eq!(t.observe(obs(T0 + 2 * MIN, "1000")), Observed::Accepted);
        assert_eq!(t.freshness(T0 + 2 * MIN), EquityFreshness::Fresh);
    }

    #[test]
    fn a_brief_spike_does_not_trip_but_n_in_a_row_does() {
        let mut t = tracker(limits());
        t.observe(obs(T0, "1000"));
        // Two readings 150 down, then recovery: streak resets, no trip.
        assert!(!tripped(t.observe(obs(T0 + MIN, "850"))));
        assert!(!tripped(t.observe(obs(T0 + 2 * MIN, "850"))));
        assert!(!tripped(t.observe(obs(T0 + 3 * MIN, "990"))));
        assert_eq!(t.is_tripped(), None);
        // Three in a row trip, on the third.
        assert!(!tripped(t.observe(obs(T0 + 4 * MIN, "850"))));
        assert!(!tripped(t.observe(obs(T0 + 5 * MIN, "850"))));
        let third = t.observe(obs(T0 + 6 * MIN, "850"));
        let Observed::Tripped(n) = third else {
            panic!("expected a trip, got {third:?}");
        };
        assert_eq!(n.account, ACCT);
        assert!(matches!(
            n.kind,
            RiskNoticeKind::Tripped {
                reason: TripReason::DailyLoss,
                ..
            }
        ));
        // Further bad readings do not report the same trip again.
        assert_eq!(t.observe(obs(T0 + 7 * MIN, "840")), Observed::Accepted);
    }

    #[test]
    fn exactly_at_the_limit_counts_as_beyond() {
        let mut t = tracker(RiskLimits {
            confirm_observations: 1,
            drawdown_bps: None,
            ..limits()
        });
        t.observe(obs(T0, "1000"));
        assert!(!tripped(t.observe(obs(T0 + MIN, "901"))));
        assert!(tripped(t.observe(obs(T0 + 2 * MIN, "900"))));
    }

    #[test]
    fn drawdown_is_measured_from_the_peak() {
        let mut t = tracker(RiskLimits {
            daily_loss_limit: None,
            confirm_observations: 1,
            ..limits()
        });
        t.observe(obs(T0, "1000"));
        t.observe(obs(T0 + MIN, "1200"));
        // 10% of 1200 is 120: 1081 is inside, 1080 is on the line.
        assert!(!tripped(t.observe(obs(T0 + 2 * MIN, "1081"))));
        let Observed::Tripped(n) = t.observe(obs(T0 + 3 * MIN, "1080")) else {
            panic!("expected a drawdown trip");
        };
        assert!(matches!(
            n.kind,
            RiskNoticeKind::Tripped {
                reason: TripReason::Drawdown,
                ..
            }
        ));
        assert_eq!(t.is_tripped(), Some(TripReason::Drawdown));
    }

    #[test]
    fn a_deposit_is_not_a_recovery_and_a_withdrawal_is_not_a_loss() {
        // Drawdown only; 10%.
        let mut t = tracker(RiskLimits {
            daily_loss_limit: None,
            confirm_observations: 1,
            ..limits()
        });
        t.observe(obs(T0, "1000"));
        // Deposit of 1000: equity 2000, peak re-based to 2000.
        assert_eq!(t.observe(transfer(T0 + MIN, "2000")), Observed::Accepted);
        // Without the re-base this is "peak 2000, equity 1900 = 5%": inside either way, so go
        // to a value that tells them apart: 1790 is 10.5% below 2000.
        assert!(tripped(t.observe(obs(T0 + 2 * MIN, "1790"))));

        // A withdrawal of 500 must not look like a 50% drawdown from the old peak 1000.
        let mut t = tracker(RiskLimits {
            daily_loss_limit: None,
            confirm_observations: 1,
            ..limits()
        });
        t.observe(obs(T0, "1000"));
        assert_eq!(t.observe(transfer(T0 + MIN, "500")), Observed::Accepted);
        assert_eq!(t.is_tripped(), None);
        // The next ordinary reading is judged against the re-based peak (500).
        assert_eq!(t.observe(obs(T0 + 2 * MIN, "480")), Observed::Accepted);
        assert!(tripped(t.observe(obs(T0 + 3 * MIN, "450"))));
    }

    #[test]
    fn a_withdrawal_does_not_count_toward_the_daily_loss() {
        let mut t = tracker(RiskLimits {
            drawdown_bps: None,
            confirm_observations: 1,
            ..limits()
        });
        t.observe(obs(T0, "1000"));
        // 300 withdrawn: equity 700, which is more than the 100 limit below the start.
        assert_eq!(t.observe(transfer(T0 + MIN, "700")), Observed::Accepted);
        assert_eq!(t.is_tripped(), None);
        // The day start is now 700, so 650 is a loss of 50 only.
        assert_eq!(t.observe(obs(T0 + 2 * MIN, "650")), Observed::Accepted);
        assert!(tripped(t.observe(obs(T0 + 3 * MIN, "600"))));
    }

    #[test]
    fn a_transfer_resets_the_streak() {
        let mut t = tracker(RiskLimits {
            drawdown_bps: None,
            ..limits()
        });
        t.observe(obs(T0, "1000"));
        t.observe(obs(T0 + MIN, "850"));
        t.observe(obs(T0 + 2 * MIN, "850"));
        t.observe(transfer(T0 + 3 * MIN, "850"));
        // One more bad reading is only the first of a new streak.
        assert!(!tripped(t.observe(obs(T0 + 4 * MIN, "700"))));
    }

    #[test]
    fn the_day_rolls_over_at_the_configured_offset() {
        // UTC+8: the boundary is 16:00 UTC.
        let l = RiskLimits {
            drawdown_bps: None,
            confirm_observations: 1,
            day_utc_offset_secs: 8 * 3600,
            ..limits()
        };
        let mut t = tracker(l);
        let before = T0 + 15 * 3_600_000; // 23:00 local
        t.observe(obs(before, "1000"));
        assert_eq!(t.observe(obs(before + 30 * MIN, "950")), Observed::Accepted);
        // 00:30 local, next day: the day start becomes 950, so 900 is only 50 down.
        let after = T0 + 16 * 3_600_000 + 30 * MIN;
        assert_eq!(t.observe(obs(after, "950")), Observed::Accepted);
        assert_eq!(t.observe(obs(after + MIN, "900")), Observed::Accepted);
        assert!(tripped(t.observe(obs(after + 2 * MIN, "850"))));

        // The two readings are on different days at UTC+8 but on the same day at UTC.
        assert_ne!(
            local_day(before, 8 * 3600),
            local_day(after, 8 * 3600),
            "the offset day boundary lies between the two readings"
        );
        assert_eq!(local_day(before, 0), local_day(after, 0));
    }

    #[test]
    fn a_daily_trip_clears_on_the_next_day_but_a_custom_anchor_trip_does_not() {
        let mut t = tracker(RiskLimits {
            drawdown_bps: None,
            confirm_observations: 1,
            ..limits()
        });
        t.observe(obs(T0, "1000"));
        assert!(tripped(t.observe(obs(T0 + MIN, "850"))));
        assert_eq!(
            verdict(&t, T0 + 2 * MIN, entry(Some("10"))),
            Err(RiskRefusal::Locked)
        );
        // Reducing is allowed while locked.
        assert_eq!(verdict(&t, T0 + 2 * MIN, Exposure::Reduces), Ok(()));
        assert_eq!(verdict(&t, T0 + 2 * MIN, Exposure::Neutral), Ok(()));
        t.observe(obs(T0 + DAY, "850"));
        assert_eq!(t.is_tripped(), None);

        let mut c = tracker(RiskLimits {
            daily_loss_limit: None,
            anchor: DrawdownAnchor::Since(T0),
            confirm_observations: 1,
            ..limits()
        });
        c.observe(obs(T0, "1000"));
        assert!(tripped(c.observe(obs(T0 + MIN, "850"))));
        c.observe(obs(T0 + DAY, "850"));
        assert_eq!(c.is_tripped(), Some(TripReason::Drawdown));
        c.acknowledge_trip();
        assert_eq!(c.is_tripped(), None);
    }

    #[test]
    fn a_custom_anchor_ignores_equity_before_its_start() {
        let mut c = tracker(RiskLimits {
            daily_loss_limit: None,
            anchor: DrawdownAnchor::Since(T0 + 10 * MIN),
            confirm_observations: 1,
            ..limits()
        });
        // A high before the start is not the peak.
        c.observe(obs(T0, "5000"));
        assert_eq!(c.observe(obs(T0 + 11 * MIN, "1000")), Observed::Accepted);
        assert_eq!(c.observe(obs(T0 + 12 * MIN, "950")), Observed::Accepted);
        assert!(tripped(c.observe(obs(T0 + 13 * MIN, "899"))));
    }

    #[test]
    fn auto_close_is_off_by_default_and_skipping_is_final_for_the_trip() {
        let mut t = tracker(RiskLimits {
            drawdown_bps: None,
            confirm_observations: 1,
            ..limits()
        });
        t.observe(obs(T0, "1000"));
        t.observe(obs(T0 + MIN, "850"));
        // The setting defaults to false: nothing runs, even on a Live account.
        assert_eq!(
            t.begin_auto_close(T0 + MIN, false),
            AutoCloseDecision::Disabled
        );
        // Turning it on afterwards does not flatten for the trip that already happened.
        assert_eq!(
            t.begin_auto_close(T0 + 2 * MIN, true),
            AutoCloseDecision::AlreadyHandled
        );
        // The entry lock stays.
        assert_eq!(
            verdict(&t, T0 + 2 * MIN, entry(Some("1"))),
            Err(RiskRefusal::Locked)
        );
    }

    #[test]
    fn auto_close_needs_fresh_equity_and_runs_once() {
        let mut t = tracker(RiskLimits {
            drawdown_bps: None,
            confirm_observations: 1,
            ..limits()
        });
        t.observe(stream(T0, "1000"));
        t.observe(stream(T0 + MIN, "850"));
        // The reading is now too old: no automatic close, and the trip is not used up.
        assert_eq!(
            t.begin_auto_close(T0 + 20 * MIN, true),
            AutoCloseDecision::EquityUnavailable
        );
        let AutoCloseDecision::Run(g) = t.begin_auto_close(T0 + 2 * MIN, true) else {
            panic!("expected the close to run");
        };
        assert_eq!(g.reason(), TripReason::DailyLoss);
        assert_eq!(g.account(), ACCT);
        assert_eq!(
            t.begin_auto_close(T0 + 2 * MIN, true),
            AutoCloseDecision::AlreadyHandled
        );
    }

    #[test]
    fn a_rest_reading_never_starts_an_automatic_close() {
        // E1: a withdrawal read over REST looks like a loss and cannot be flagged.
        let mut t = tracker(RiskLimits {
            drawdown_bps: None,
            confirm_observations: 1,
            ..limits()
        });
        t.observe(obs(T0, "1000"));
        // 1,000 moved to the funding account: equity drops by 1,000 with no transfer flag.
        assert!(tripped(t.observe(obs(T0 + MIN, "1"))));
        assert_eq!(
            t.begin_auto_close(T0 + MIN, true),
            AutoCloseDecision::TransferBlind
        );
        // Decided for this trip: it does not come back later, but the entry lock stays.
        assert_eq!(
            t.begin_auto_close(T0 + MIN, true),
            AutoCloseDecision::AlreadyHandled
        );
        assert_eq!(
            verdict(&t, T0 + MIN, entry(Some("1"))),
            Err(RiskRefusal::Locked)
        );
    }

    #[test]
    fn a_source_that_flags_transfers_may_start_an_automatic_close() {
        let mut t = tracker(RiskLimits {
            drawdown_bps: None,
            confirm_observations: 1,
            ..limits()
        });
        t.observe(stream(T0, "1000"));
        assert!(tripped(t.observe(stream(T0 + MIN, "850"))));
        assert!(matches!(
            t.begin_auto_close(T0 + MIN, true),
            AutoCloseDecision::Run(_)
        ));

        // A REST reading after the stream ones takes the capability away again.
        let mut r = tracker(RiskLimits {
            drawdown_bps: None,
            confirm_observations: 1,
            ..limits()
        });
        r.observe(stream(T0, "1000"));
        assert!(tripped(r.observe(obs(T0 + MIN, "850"))));
        assert_eq!(
            r.begin_auto_close(T0 + MIN, true),
            AutoCloseDecision::TransferBlind
        );
    }

    #[test]
    fn a_daily_loss_trip_lifted_today_does_not_hand_out_a_second_grant() {
        // E3: lifting the trip keeps the day's start, so the loss is still beyond the limit.
        let mut t = tracker(RiskLimits {
            drawdown_bps: None,
            confirm_observations: 1,
            ..limits()
        });
        t.observe(stream(T0, "1000"));
        assert!(tripped(t.observe(stream(T0 + MIN, "850"))));
        assert!(matches!(
            t.begin_auto_close(T0 + MIN, true),
            AutoCloseDecision::Run(_)
        ));
        t.acknowledge_trip();
        assert_eq!(t.is_tripped(), None);
        // Still down more than the limit from the day's start: trips again, but no grant.
        assert!(tripped(t.observe(stream(T0 + 2 * MIN, "840"))));
        assert_eq!(
            t.begin_auto_close(T0 + 2 * MIN, true),
            AutoCloseDecision::AlreadyHandled
        );
        assert_eq!(
            verdict(&t, T0 + 2 * MIN, entry(Some("1"))),
            Err(RiskRefusal::Locked)
        );

        // Next day the memory is gone: a new day's loss is handled as usual.
        t.observe(stream(T0 + DAY, "840"));
        assert_eq!(t.is_tripped(), None);
        assert!(tripped(t.observe(stream(T0 + DAY + MIN, "700"))));
        assert!(matches!(
            t.begin_auto_close(T0 + DAY + MIN, true),
            AutoCloseDecision::Run(_)
        ));
    }

    #[test]
    fn entry_caps_refuse_unknown_and_excess_values() {
        let mut t = tracker(RiskLimits {
            max_order_notional: Some(d("1000")),
            ..RiskLimits::default()
        });
        // No equity limit set, so a missing reading is not a block.
        assert_eq!(verdict(&t, T0, entry(Some("1000"))), Ok(()));
        assert_eq!(
            verdict(&t, T0, entry(Some("1000.01"))),
            Err(RiskRefusal::SingleOrderCapExceeded)
        );
        // Review item 16: a cap is set, the value is unknown: refused.
        assert_eq!(
            verdict(&t, T0, entry(None)),
            Err(RiskRefusal::NotionalUnknown)
        );
        // Review item 16 holds with no cap set too: an unvalued entry is never waved through.
        t.set_limits(RiskLimits::default());
        assert_eq!(
            verdict(&t, T0, entry(None)),
            Err(RiskRefusal::NotionalUnknown)
        );
        assert_eq!(verdict(&t, T0, entry(Some("1"))), Ok(()));
    }

    #[test]
    fn symbol_cap_counts_the_position_already_held() {
        let mut t = tracker(RiskLimits {
            max_symbol_notional: Some(d("5000")),
            ..RiskLimits::default()
        });
        // No position reading at all.
        assert_eq!(
            verdict(&t, T0, entry(Some("100"))),
            Err(RiskRefusal::PositionUnknown)
        );
        t.set_positions(T0, [("BTC-USDT-SWAP".to_string(), d("4500"))]);
        assert_eq!(verdict(&t, T0 + MIN, entry(Some("500"))), Ok(()));
        assert_eq!(
            verdict(&t, T0 + MIN, entry(Some("500.01"))),
            Err(RiskRefusal::SymbolCapExceeded)
        );
        // Another symbol has no position: only the order counts.
        let other = t.snapshot(T0 + MIN).check(&RiskView {
            account: ACCT,
            symbol: "ETH-USDT-SWAP",
            exposure: entry(Some("5000")),
        });
        assert_eq!(other, Ok(()));
        // The position reading goes stale.
        assert_eq!(
            verdict(&t, T0 + 10 * MIN, entry(Some("1"))),
            Err(RiskRefusal::PositionUnknown)
        );
        // Reducing is never judged by the cap.
        assert_eq!(verdict(&t, T0 + 10 * MIN, Exposure::Reduces), Ok(()));
    }

    #[test]
    fn another_accounts_order_is_refused() {
        let t = tracker(RiskLimits::default());
        let r = t.snapshot(T0).check(&RiskView {
            account: AccountKey::from_bytes([9; 16]),
            symbol: "BTC-USDT",
            exposure: Exposure::Reduces,
        });
        assert_eq!(r, Err(RiskRefusal::WrongAccount));
    }

    #[test]
    fn minimum_confirmation_is_one() {
        let mut t = tracker(RiskLimits {
            drawdown_bps: None,
            confirm_observations: 0,
            ..limits()
        });
        t.observe(obs(T0, "1000"));
        assert!(tripped(t.observe(obs(T0 + MIN, "800"))));
    }

    #[test]
    fn changing_the_anchor_restarts_the_peak() {
        let mut t = tracker(RiskLimits {
            daily_loss_limit: None,
            confirm_observations: 1,
            ..limits()
        });
        t.observe(obs(T0, "5000"));
        assert_eq!(t.observe(obs(T0 + MIN, "4600")), Observed::Accepted);
        // A custom start now: the old high of 5000 must not count.
        t.set_limits(RiskLimits {
            daily_loss_limit: None,
            anchor: DrawdownAnchor::Since(T0),
            confirm_observations: 1,
            ..limits()
        });
        assert_eq!(t.observe(obs(T0 + 2 * MIN, "4500")), Observed::Accepted);
        assert_eq!(t.is_tripped(), None);
    }

    /// Condition 72: what the gate lets through, for every state and every kind of order.
    #[test]
    fn the_hook_table_state_by_order_kind() {
        let mut clear = tracker(limits());
        clear.observe(obs(T0, "1000"));
        let mut locked = tracker(RiskLimits {
            drawdown_bps: None,
            confirm_observations: 1,
            ..limits()
        });
        locked.observe(obs(T0, "1000"));
        assert!(tripped(locked.observe(obs(T0 + MIN, "800"))));
        let mut stale = tracker(limits());
        stale.observe(obs(T0, "1000"));
        let unknown = tracker(limits());

        let now = T0 + MIN;
        let late = T0 + 30 * MIN;
        // (name, tracker, now, adds known, adds unknown value, reduces, neutral)
        type Row<'a> = (
            &'a str,
            &'a EquityTracker,
            i64,
            [Result<(), RiskRefusal>; 4],
        );
        let ok = Ok(());
        let rows: [Row<'_>; 4] = [
            (
                "clear",
                &clear,
                now,
                [ok, Err(RiskRefusal::NotionalUnknown), ok, ok],
            ),
            (
                "locked",
                &locked,
                now,
                [Err(RiskRefusal::Locked), Err(RiskRefusal::Locked), ok, ok],
            ),
            (
                "stale",
                &stale,
                late,
                [
                    Err(RiskRefusal::EquityUnavailable),
                    Err(RiskRefusal::EquityUnavailable),
                    ok,
                    ok,
                ],
            ),
            (
                "unknown",
                &unknown,
                now,
                [
                    Err(RiskRefusal::EquityUnavailable),
                    Err(RiskRefusal::EquityUnavailable),
                    ok,
                    ok,
                ],
            ),
        ];
        for (name, t, at, want) in rows {
            let got = [
                verdict(t, at, entry(Some("10"))),
                verdict(t, at, entry(None)),
                verdict(t, at, Exposure::Reduces),
                verdict(t, at, Exposure::Neutral),
            ];
            assert_eq!(got, want, "{name}");
        }
    }
}
