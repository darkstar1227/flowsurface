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
use super::stream::{PrivateEvent, StreamKey};
use crate::account::{AccountKey, AccountProduct, AccountVenue};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

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
    /// The private stream, from a venue whose stream tells transfers apart (Binance
    /// `ACCOUNT_UPDATE` reasons). Only [`StreamEquityFeed`] makes these, and only the tracker
    /// fed by them may start an automatic close (E1).
    PrivateStream,
    /// The private stream, from a venue whose equity push carries no transfer information
    /// (OKX `account` channel). It is as blind to a withdrawal as a REST read.
    StreamWithoutTransferFlags,
}

/// One reading of the account's equity, in the quote currency of the account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
///
/// The fields are crate-private: a caller outside `exchange` cannot claim that a reading came
/// from the private stream or that it was a transfer. REST readings come from
/// [`EquityObservation::rest`]; stream readings only from [`StreamEquityFeed`].
///
/// ```compile_fail,E0451
/// use flowsurface_exchange::trade::{EquityBasis, EquityObservation, EquitySource, Decimal};
/// // No struct literal outside the crate: a reading cannot claim to be from the stream.
/// fn forge(d: Decimal) -> EquityObservation {
///     EquityObservation { at_ms: 0, equity: d, basis: EquityBasis::OkxTotalEq,
///         source: EquitySource::PrivateStream, is_transfer: false }
/// }
/// ```
///
/// ```compile_fail,E0639
/// use flowsurface_exchange::trade::stream::EquityUpdate;
/// use flowsurface_exchange::trade::Decimal;
/// // `EquityUpdate` is non_exhaustive: only the stream parsers build one.
/// fn forge(d: Decimal) -> EquityUpdate { EquityUpdate { total_equity: d, transfer_flags: true } }
/// ```
pub struct EquityObservation {
    pub(crate) at_ms: i64,
    pub(crate) equity: Decimal,
    pub(crate) basis: EquityBasis,
    pub(crate) source: EquitySource,
    /// The change since the previous observation came from a deposit or withdrawal
    /// (`ACCOUNT_UPDATE` reason, or an OKX funding bill). It re-bases the day start and the
    /// peak and is not counted as a loss or a recovery.
    pub(crate) is_transfer: bool,
}

impl EquityObservation {
    /// A REST reading: it cannot flag transfers.
    pub fn rest(at_ms: i64, equity: Decimal, basis: EquityBasis) -> Self {
        Self {
            at_ms,
            equity,
            basis,
            source: EquitySource::Rest,
            is_transfer: false,
        }
    }

    pub fn at_ms(&self) -> i64 {
        self.at_ms
    }

    pub fn equity(&self) -> Decimal {
        self.equity
    }

    pub fn basis(&self) -> EquityBasis {
        self.basis
    }

    pub fn source(&self) -> EquitySource {
        self.source
    }

    pub fn is_transfer(&self) -> bool {
        self.is_transfer
    }
}

/// Turns private-stream events into equity readings for one account (review item 16).
///
/// An `Account(reason)` event marks the next equity reading as a transfer when the reason is
/// a deposit, withdrawal or transfer; an `Equity` event then yields one observation. Venues
/// whose equity push has no transfer information (OKX) yield
/// [`EquitySource::StreamWithoutTransferFlags`], so they never make the tracker
/// transfer-aware (E1 stays true). Positions are not read from here: they come from
/// `Position` events and the snapshot only.
///
/// What a reading means is not up to the caller (review B2): the basis follows from the stream's
/// venue and product, and whether the venue tells transfers apart is set by the parser on each
/// event. There is no argument to claim otherwise.
///
/// ```compile_fail
/// use flowsurface_exchange::trade::{EquityBasis, StreamEquityFeed};
/// // The old two-argument form, with a caller-chosen transfer flag, no longer exists.
/// fn claim() -> StreamEquityFeed { StreamEquityFeed::new(EquityBasis::OkxTotalEq, true) }
/// ```
#[derive(Debug)]
pub struct StreamEquityFeed {
    basis: EquityBasis,
    /// OKX never tells a transfer apart, whatever an event says.
    venue_flags_transfers: bool,
    transfer_pending: bool,
}

impl StreamEquityFeed {
    /// The feed for the private stream `key` names.
    pub fn new(key: &StreamKey) -> Self {
        Self {
            basis: match (key.venue, key.product) {
                (AccountVenue::Okx, _) => EquityBasis::OkxTotalEq,
                (AccountVenue::Binance, AccountProduct::UsdM) => {
                    EquityBasis::BinanceTotalMarginBalance
                }
                (AccountVenue::Binance, AccountProduct::Spot) => EquityBasis::SpotMarkValued,
            },
            venue_flags_transfers: key.venue != AccountVenue::Okx,
            transfer_pending: false,
        }
    }

    /// Feed one private event; `at_ms` is the client's clock at receipt.
    pub fn on_event(&mut self, event: &PrivateEvent, at_ms: i64) -> Option<EquityObservation> {
        match event {
            PrivateEvent::Account(reason) => {
                if reason.is_transfer() {
                    self.transfer_pending = true;
                }
                None
            }
            PrivateEvent::Equity(update) => {
                let is_transfer = std::mem::take(&mut self.transfer_pending);
                Some(EquityObservation {
                    at_ms,
                    equity: update.total_equity(),
                    basis: self.basis,
                    source: if update.transfer_flags && self.venue_flags_transfers {
                        EquitySource::PrivateStream
                    } else {
                        EquitySource::StreamWithoutTransferFlags
                    },
                    is_transfer,
                })
            }
            _ => None,
        }
    }
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
    /// The saved risk settings could not be turned into limits: new entries are refused until
    /// valid limits are set (a bad setting never means "no limits").
    settings_invalid: bool,
    /// The sync epoch of the last snapshot the caller applied (0 = none). New entries need it
    /// to equal the account's stream epoch (see `stream::sync_epoch`).
    applied_epoch: u64,
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
            settings_invalid: false,
            applied_epoch: 0,
        }
    }

    /// A tracker for an account whose saved risk settings are invalid. It has no limits, but
    /// it refuses every new entry ([`RiskRefusal::SettingsInvalid`]) until
    /// [`EquityTracker::set_limits`] is given valid ones. Reducing and cancelling still pass.
    pub fn with_invalid_settings(account: AccountKey, basis: EquityBasis) -> Self {
        let mut t = Self::new(account, basis, RiskLimits::default());
        t.settings_invalid = true;
        t
    }

    pub fn settings_invalid(&self) -> bool {
        self.settings_invalid
    }

    /// The private stream delivered a REST snapshot (a reconnect and resync). The snapshot
    /// carries no equity, so the equity reference is not trusted until a new reading
    /// arrives: the tracker counts as stale (new entries under an equity limit are refused
    /// and no automatic close starts) until the caller reads the equity again and feeds it.
    ///
    /// The tracker takes the snapshot itself: the epoch is read from it here and the position
    /// book is set in the same call (`as_of_ms` and `notionals` as in [`Self::set_positions`];
    /// a snapshot carries sizes, not prices, so the caller supplies the notionals). A bare
    /// number is not accepted, so a caller cannot pass `sync_epoch(account)` without having
    /// applied anything:
    ///
    /// ```compile_fail,E0308
    /// use flowsurface_exchange::trade::EquityTracker;
    /// fn apply(tracker: &mut EquityTracker) {
    ///     tracker.snapshot_applied(0, 0, []);
    /// }
    /// ```
    ///
    /// The snapshot cannot be forged outside the crate either (its epoch is crate-private):
    ///
    /// ```compile_fail,E0451
    /// use flowsurface_exchange::trade::stream::Snapshot;
    /// let _ = Snapshot { orders: vec![], positions: vec![], balances: vec![], epoch: 0 };
    /// ```
    ///
    /// The stream reports `Connected` as soon as it has *sent* a snapshot, possibly before
    /// this call: until the tracker has applied the account's latest snapshot, new entries are
    /// refused with [`RiskRefusal::SnapshotNotApplied`].
    pub fn snapshot_applied(
        &mut self,
        snapshot: &super::stream::Snapshot,
        as_of_ms: i64,
        notionals: impl IntoIterator<Item = (String, Decimal)>,
    ) {
        self.set_positions(as_of_ms, notionals);
        self.degraded = true;
        self.applied_epoch = snapshot.sync_epoch();
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
        self.settings_invalid = false;
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
    ///
    /// Keys are the exchange's own symbols (`BTCUSDT`, `BTC-USDT-SWAP`), the same format as
    /// `RiskView.symbol` and `Position.symbol`. A key that is not a valid symbol means the
    /// reading cannot be matched to orders: the whole book is dropped and the position counts
    /// as unknown (entries under a per-symbol cap are refused) instead of silently missing.
    ///
    /// The cap counts the held position plus the new order only. Resting entry orders that
    /// have not filled yet are not counted (decision on review item L10): the tracker has no
    /// price for them, and counting them with a guess would be worse than the plain rule.
    /// Residual risk: several resting entries can together exceed the cap once they fill.
    pub fn set_positions(
        &mut self,
        as_of_ms: i64,
        notionals: impl IntoIterator<Item = (String, Decimal)>,
    ) {
        let notionals: HashMap<String, Decimal> = notionals.into_iter().collect();
        if notionals
            .keys()
            .any(|k| crate::signing::validate_symbol(k).is_err())
        {
            self.positions = None;
            return;
        }
        self.positions = Some(PositionBook {
            as_of_ms,
            notionals,
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

impl super::gate::sealed::Sealed for RiskSnapshot<'_> {}

impl RiskHook for RiskSnapshot<'_> {
    fn check(&self, view: &RiskView<'_>) -> Result<(), RiskRefusal> {
        check_entry(self.tracker, self.now_ms, view)
    }
}

/// The one risk check, shared by [`RiskSnapshot`] and [`SharedRisk`] so they cannot drift.
fn check_entry(t: &EquityTracker, now_ms: i64, view: &RiskView<'_>) -> Result<(), RiskRefusal> {
    if view.account != t.account {
        return Err(RiskRefusal::WrongAccount);
    }
    // Only entries are judged: reducing, closing and cancelling always pass (condition 72).
    let Exposure::Adds { notional } = view.exposure else {
        return Ok(());
    };
    // The stream is `Connected` as soon as it sent its snapshot; the positions and equity
    // this tracker holds are from before it until the caller applied that snapshot.
    if super::stream::sync_epoch(t.account) != t.applied_epoch {
        return Err(RiskRefusal::SnapshotNotApplied);
    }
    if t.settings_invalid {
        return Err(RiskRefusal::SettingsInvalid);
    }
    if t.trip.is_some() {
        return Err(RiskRefusal::Locked);
    }
    if t.limits.equity_limits_set() && t.freshness(now_ms) != EquityFreshness::Fresh {
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
        let age = now_ms.saturating_sub(book.as_of_ms);
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

/// The risk tracker shared between the GUI and the tasks that send: the second sealed
/// [`RiskHook`]. `RiskSnapshot` borrows the tracker, so it cannot be moved into a `'static`
/// future; this holds the tracker behind a lock instead, and reads the clock itself.
///
/// The lock is taken inside each method and never held across an `.await` (nothing here is
/// async). A poisoned lock fails closed: entries are refused with
/// [`RiskRefusal::StateUnavailable`], reducing, closing and cancelling still pass (as in
/// `RiskSnapshot`, condition 72). The mutating methods report the poisoning instead of
/// guessing.
#[derive(Clone)]
pub struct SharedRisk {
    /// The tracker's account, kept outside the lock so a poisoned lock still checks it.
    account: AccountKey,
    tracker: Arc<Mutex<EquityTracker>>,
    clock: Arc<dyn Fn() -> i64 + Send + Sync>,
}

impl std::fmt::Debug for SharedRisk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SharedRisk(..)")
    }
}

/// The shared tracker's lock was poisoned by a panic: its state cannot be trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RiskPoisoned;

impl SharedRisk {
    /// `clock` returns the current time in unix milliseconds (injected so tests need no wall
    /// clock).
    pub fn new(tracker: EquityTracker, clock: impl Fn() -> i64 + Send + Sync + 'static) -> Self {
        Self {
            account: tracker.account,
            tracker: Arc::new(Mutex::new(tracker)),
            clock: Arc::new(clock),
        }
    }

    /// Run `f` on the tracker under the lock. `f` is synchronous: the lock cannot outlive it.
    pub fn with_tracker<R>(
        &self,
        f: impl FnOnce(&mut EquityTracker) -> R,
    ) -> Result<R, RiskPoisoned> {
        let mut guard = self.tracker.lock().map_err(|_| RiskPoisoned)?;
        Ok(f(&mut guard))
    }

    /// [`EquityTracker::snapshot_applied`] under the lock.
    pub fn snapshot_applied(
        &self,
        snapshot: &super::stream::Snapshot,
        as_of_ms: i64,
        notionals: impl IntoIterator<Item = (String, Decimal)>,
    ) -> Result<(), RiskPoisoned> {
        self.with_tracker(|t| t.snapshot_applied(snapshot, as_of_ms, notionals))
    }

    /// [`EquityTracker::observe`] under the lock.
    pub fn observe(&self, obs: EquityObservation) -> Result<Observed, RiskPoisoned> {
        self.with_tracker(|t| t.observe(obs))
    }

    /// [`EquityTracker::set_positions`] under the lock.
    pub fn set_positions(
        &self,
        as_of_ms: i64,
        notionals: impl IntoIterator<Item = (String, Decimal)>,
    ) -> Result<(), RiskPoisoned> {
        self.with_tracker(|t| t.set_positions(as_of_ms, notionals))
    }

    /// [`EquityTracker::set_limits`] under the lock.
    pub fn set_limits(&self, limits: RiskLimits) -> Result<(), RiskPoisoned> {
        self.with_tracker(|t| t.set_limits(limits))
    }

    /// [`EquityTracker::acknowledge_trip`] under the lock.
    pub fn acknowledge_trip(&self) -> Result<(), RiskPoisoned> {
        self.with_tracker(|t| t.acknowledge_trip())
    }

    /// [`EquityTracker::is_tripped`] under the lock.
    pub fn is_tripped(&self) -> Result<Option<TripReason>, RiskPoisoned> {
        self.with_tracker(|t| t.is_tripped())
    }

    /// [`EquityTracker::freshness`] at the shared clock's now.
    pub fn freshness(&self) -> Result<EquityFreshness, RiskPoisoned> {
        let now = (self.clock)();
        self.with_tracker(|t| t.freshness(now))
    }

    /// [`EquityTracker::begin_auto_close`] at the shared clock's now.
    pub fn begin_auto_close(&self, enabled: bool) -> Result<AutoCloseDecision, RiskPoisoned> {
        let now = (self.clock)();
        self.with_tracker(|t| t.begin_auto_close(now, enabled))
    }
}

impl super::gate::sealed::Sealed for SharedRisk {}

impl RiskHook for SharedRisk {
    fn check(&self, view: &RiskView<'_>) -> Result<(), RiskRefusal> {
        let now_ms = (self.clock)();
        let Ok(t) = self.tracker.lock() else {
            // Poisoned: only entries are refused, like every other refusal here. The account is
            // still checked, as in `check_entry`.
            if view.account != self.account {
                return Err(RiskRefusal::WrongAccount);
            }
            return match view.exposure {
                Exposure::Adds { .. } => Err(RiskRefusal::StateUnavailable),
                Exposure::Reduces | Exposure::Neutral => Ok(()),
            };
        };
        check_entry(&t, now_ms, view)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(epoch: u64) -> crate::trade::stream::Snapshot {
        crate::trade::stream::Snapshot {
            orders: vec![],
            positions: vec![],
            balances: vec![],
            epoch,
        }
    }

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
    fn position_keys_use_the_symbol_format_of_the_order_and_a_bad_key_means_unknown() {
        let mut t = tracker(RiskLimits {
            max_symbol_notional: Some(d("5000")),
            ..RiskLimits::default()
        });
        let entry = |symbol| RiskView {
            account: ACCT,
            symbol,
            exposure: Exposure::Adds {
                notional: Some(d("500")),
            },
        };
        // Binance and OKX symbols as `Position.symbol` and `OrderIntent::symbol` carry them.
        t.set_positions(
            T0,
            [
                ("BTCUSDT".to_string(), d("4600")),
                ("BTC-USDT-SWAP".to_string(), d("4600")),
            ],
        );
        for sym in ["BTCUSDT", "BTC-USDT-SWAP"] {
            assert_eq!(
                t.snapshot(T0).check(&entry(sym)),
                Err(RiskRefusal::SymbolCapExceeded),
                "{sym}"
            );
        }
        // A key in another format cannot be matched: the book is dropped, not half used.
        t.set_positions(T0, [("btc/usdt".to_string(), d("4600"))]);
        assert_eq!(
            t.snapshot(T0).check(&entry("BTCUSDT")),
            Err(RiskRefusal::PositionUnknown)
        );
    }

    fn equity_event(v: &str) -> PrivateEvent {
        PrivateEvent::Equity(super::super::stream::EquityUpdate::new(d(v), false))
    }

    fn equity_event_flagged(v: &str) -> PrivateEvent {
        PrivateEvent::Equity(super::super::stream::EquityUpdate::new(d(v), true))
    }

    use crate::account::AccountEnv;

    fn key_of(venue: AccountVenue, product: AccountProduct) -> StreamKey {
        StreamKey {
            account: AccountKey::from_bytes([9; 16]),
            venue,
            env: AccountEnv::Live,
            product,
            generation: 0,
        }
    }

    #[test]
    fn stream_feed_flags_a_transfer_once_and_only_where_the_venue_reports_reasons() {
        use super::super::stream::AccountReason;
        let mut binance =
            StreamEquityFeed::new(&key_of(AccountVenue::Binance, AccountProduct::UsdM));
        // Other events yield nothing.
        assert!(
            binance
                .on_event(&PrivateEvent::Account(AccountReason::Order), 1)
                .is_none()
        );
        let plain = binance.on_event(&equity_event_flagged("100"), 2).unwrap();
        assert_eq!(plain.source(), EquitySource::PrivateStream);
        assert!(!plain.is_transfer());
        binance.on_event(&PrivateEvent::Account(AccountReason::Withdraw), 3);
        let moved = binance.on_event(&equity_event_flagged("60"), 4).unwrap();
        assert!(moved.is_transfer());
        // The flag is used up by one reading.
        assert!(
            !binance
                .on_event(&equity_event_flagged("61"), 5)
                .unwrap()
                .is_transfer()
        );

        // OKX: no transfer information, so the reading is not "private stream" for E1.
        let mut okx = StreamEquityFeed::new(&key_of(AccountVenue::Okx, AccountProduct::UsdM));
        let o = okx.on_event(&equity_event("100"), 6).unwrap();
        assert_eq!(o.source(), EquitySource::StreamWithoutTransferFlags);
        assert_eq!(o.basis(), EquityBasis::OkxTotalEq);
        // Even an update that claimed flags would not make an OKX reading transfer-aware.
        let claimed = okx.on_event(&equity_event_flagged("100"), 7).unwrap();
        assert_eq!(claimed.source(), EquitySource::StreamWithoutTransferFlags);
        assert_eq!(
            StreamEquityFeed::new(&key_of(AccountVenue::Binance, AccountProduct::Spot)).basis,
            EquityBasis::SpotMarkValued
        );
    }

    #[test]
    fn e1_an_okx_stream_reading_never_makes_the_tracker_transfer_aware() {
        let mut t = tracker(RiskLimits::default());
        let mut okx = StreamEquityFeed::new(&key_of(AccountVenue::Okx, AccountProduct::UsdM));
        let o = okx.on_event(&equity_event("100"), T0).unwrap();
        t.observe(o);
        t.observe(obs(T0 + MIN, "99"));
        assert_eq!(
            t.begin_auto_close(T0 + MIN, true),
            AutoCloseDecision::NotTripped,
            "nothing tripped, so nothing to close"
        );
        // A trip on this source is still refused for the automatic close.
        let mut t = tracker(RiskLimits {
            daily_loss_limit: Some(d("10")),
            ..RiskLimits::default()
        });
        t.observe(okx.on_event(&equity_event("100"), T0).unwrap());
        for n in 1..=10 {
            t.observe(okx.on_event(&equity_event("50"), T0 + n * MIN).unwrap());
        }
        assert_eq!(
            t.begin_auto_close(T0 + 10 * MIN, true),
            AutoCloseDecision::TransferBlind
        );
    }

    #[test]
    fn a_snapshot_makes_equity_stale_until_a_new_reading() {
        let mut t = tracker(RiskLimits {
            daily_loss_limit: Some(d("500")),
            ..RiskLimits::default()
        });
        t.observe(obs(T0, "1000"));
        assert_eq!(t.freshness(T0 + 1), EquityFreshness::Fresh);
        t.snapshot_applied(&snapshot(0), T0, []);
        assert_eq!(t.freshness(T0 + 1), EquityFreshness::Stale);
        t.observe(obs(T0 + MIN, "1000"));
        assert_eq!(t.freshness(T0 + MIN), EquityFreshness::Fresh);
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

    #[test]
    fn applying_a_snapshot_sets_the_positions_and_the_epoch_together() {
        let mut t = tracker(RiskLimits::default());
        t.snapshot_applied(&snapshot(4), T0, [("BTC-USDT-SWAP".to_string(), d("4500"))]);
        assert_eq!(t.applied_epoch, 4);
        assert!(t.positions.is_some());
        // A position under an invalid symbol makes the book unknown, but the epoch counts.
        t.snapshot_applied(&snapshot(5), T0, [("btc/usdt".to_string(), d("1"))]);
        assert_eq!(t.applied_epoch, 5);
        assert!(t.positions.is_none());
    }

    // ── SharedRisk ──────────────────────────────────────────────────────

    /// The same tracker twice: one is read through `RiskSnapshot`, the other through
    /// `SharedRisk`. Each scenario returns a fresh pair of identical trackers.
    fn scenario(n: usize) -> EquityTracker {
        match n {
            0 => tracker(RiskLimits::default()),
            1 => tracker(RiskLimits {
                max_order_notional: Some(d("1000")),
                ..RiskLimits::default()
            }),
            // An equity limit and no reading yet.
            2 => tracker(limits()),
            3 => EquityTracker::with_invalid_settings(ACCT, EquityBasis::OkxTotalEq),
            4 => {
                let mut t = tracker(RiskLimits {
                    max_symbol_notional: Some(d("2000")),
                    ..RiskLimits::default()
                });
                t.set_positions(T0, [("BTC-USDT-SWAP".to_string(), d("1500"))]);
                t
            }
            // A fresh reading under a limit, then a reading far below: trips.
            _ => {
                let mut t = tracker(RiskLimits {
                    daily_loss_limit: Some(d("100")),
                    confirm_observations: 1,
                    ..RiskLimits::default()
                });
                t.observe(EquityObservation::rest(
                    T0,
                    d("1000"),
                    EquityBasis::OkxTotalEq,
                ));
                t.observe(EquityObservation::rest(
                    T0 + MIN,
                    d("800"),
                    EquityBasis::OkxTotalEq,
                ));
                t
            }
        }
    }

    #[test]
    fn the_shared_hook_judges_exactly_like_the_snapshot() {
        let exposures = [
            entry(Some("500")),
            entry(Some("1500")),
            entry(None),
            Exposure::Reduces,
            Exposure::Neutral,
        ];
        let accounts = [ACCT, AccountKey::from_bytes([9; 16])];
        for n in 0..6 {
            for now in [T0 + MIN, T0 + 2 * MIN, T0 + DAY] {
                let alone = scenario(n);
                let shared = SharedRisk::new(scenario(n), move || now);
                for account in accounts {
                    for exposure in exposures {
                        let view = RiskView {
                            account,
                            symbol: "BTC-USDT-SWAP",
                            exposure,
                        };
                        assert_eq!(
                            alone.snapshot(now).check(&view),
                            shared.check(&view),
                            "scenario {n}, now {now}, {exposure:?}, account {account:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn the_shared_hook_reads_the_clock_on_every_check() {
        use std::sync::atomic::{AtomicI64, Ordering as AO};
        let now = Arc::new(AtomicI64::new(T0));
        let clock = now.clone();
        let shared = SharedRisk::new(scenario(4), move || clock.load(AO::Relaxed));
        let view = RiskView {
            account: ACCT,
            symbol: "BTC-USDT-SWAP",
            exposure: entry(Some("100")),
        };
        assert_eq!(shared.check(&view), Ok(()));
        // The position reading is now older than `max_equity_age_ms`.
        now.store(T0 + DAY, AO::Relaxed);
        assert_eq!(shared.check(&view), Err(RiskRefusal::PositionUnknown));
    }

    #[test]
    fn a_snapshot_applied_through_the_shared_hook_is_seen_by_the_next_check() {
        let shared = SharedRisk::new(scenario(0), || T0);
        let clone = shared.clone();
        let snap = snapshot(7);
        clone
            .snapshot_applied(&snap, T0, [("BTC-USDT-SWAP".to_string(), d("10"))])
            .unwrap();
        // One tracker behind both handles: the clone's write is visible to the original.
        let epoch = shared.with_tracker(|t| t.applied_epoch).unwrap();
        assert_eq!(epoch, 7);
        // The stream of this account is at epoch 0 (not registered), so the entry is refused
        // exactly as `RiskSnapshot` refuses it.
        let view = RiskView {
            account: ACCT,
            symbol: "BTC-USDT-SWAP",
            exposure: entry(Some("1")),
        };
        assert_eq!(
            shared.check(&view),
            shared
                .with_tracker(|t| t.snapshot(T0).check(&view))
                .unwrap()
        );
        assert_eq!(shared.check(&view), Err(RiskRefusal::SnapshotNotApplied));
    }

    #[test]
    fn a_poisoned_shared_hook_refuses_entries_only_and_reports_the_poisoning() {
        let shared = SharedRisk::new(scenario(0), || T0);
        let victim = shared.clone();
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = victim.with_tracker(|_| panic!("poison the lock on purpose"));
        }));
        assert!(panicked.is_err());
        let view = |exposure| RiskView {
            account: ACCT,
            symbol: "BTC-USDT-SWAP",
            exposure,
        };
        assert_eq!(
            shared.check(&view(entry(Some("1")))),
            Err(RiskRefusal::StateUnavailable)
        );
        assert_eq!(shared.check(&view(Exposure::Reduces)), Ok(()));
        assert_eq!(shared.check(&view(Exposure::Neutral)), Ok(()));
        // Same account check as the healthy path and `RiskSnapshot`.
        let other = RiskView {
            account: AccountKey::from_bytes([0xEE; 16]),
            symbol: "BTC-USDT-SWAP",
            exposure: Exposure::Reduces,
        };
        assert_eq!(shared.check(&other), Err(RiskRefusal::WrongAccount));
        assert_eq!(
            scenario(0).snapshot(T0).check(&other),
            Err(RiskRefusal::WrongAccount)
        );
        assert_eq!(shared.freshness(), Err(RiskPoisoned));
        assert_eq!(shared.acknowledge_trip(), Err(RiskPoisoned));
    }

    #[test]
    fn the_shared_hook_is_send_sync_static_and_a_hook_of_it_is_a_send_future() {
        fn needs<T: Send + Sync + 'static>() {}
        needs::<SharedRisk>();
        // Taken by a spawned task: the check is plain data work, not an await point.
        let shared = SharedRisk::new(scenario(0), || T0);
        let fut = async move {
            let view = RiskView {
                account: ACCT,
                symbol: "BTC-USDT-SWAP",
                exposure: Exposure::Neutral,
            };
            shared.check(&view)
        };
        fn send_static<F: std::future::Future + Send + 'static>(_: &F) {}
        send_static(&fut);
    }
}
