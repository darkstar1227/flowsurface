//! Protection status of positions and protective-order arithmetic (M5, stage d1).
//!
//! Pure functions over what the private stream read: no I/O, nothing is sent. They answer
//! three questions the later stages build on:
//!
//! * [`protected_status`]: does each position have a stop (user decision: only a stop counts
//!   as protected; a take-profit alone is "no stop")?
//! * [`orphan_protectives`]: which protective orders have no position to protect?
//! * [`plan`]: from a fill price and the defaults, which trigger prices and quantities?
//!
//! Nothing here guesses. A list that may be stale gives [`Protection::Undetermined`], never
//! "protected"; an order row that lacks a field protects nothing.

use super::client::{OpenOrder, Position};
use super::decimal::{Decimal, DecimalError};
use super::types::{Dir, Side};
use std::cmp::Ordering;

/// What kind of conditional order a row is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtectRole {
    Stop,
    Target,
    /// An OKX order that carries both sides (`oco`, or `conditional` with both triggers).
    StopAndTarget,
    Trailing,
}

impl ProtectRole {
    pub fn has_stop(self) -> bool {
        // A trailing stop is not counted: it may carry an activation price that the price has
        // not reached, and a row cannot tell whether it is active yet.
        matches!(self, ProtectRole::Stop | ProtectRole::StopAndTarget)
    }

    pub fn has_target(self) -> bool {
        matches!(self, ProtectRole::Target | ProtectRole::StopAndTarget)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protection {
    /// A stop covers the whole position.
    Stop,
    /// A stop exists but its size is smaller than the position, or its size is unknown.
    PartialStop,
    /// Only a trailing stop, which may not be active yet (activation price): not protected.
    TrailingOnly,
    /// Only take-profit orders (lower severity than no protection at all).
    TargetOnly,
    Unprotected,
    /// The lists may not match the exchange: nothing can be said.
    Undetermined,
    /// The entry has protection attached that is not in force yet (OKX attaches it to the
    /// entry; on Binance the app sends it after the fill). Set by the GUI, never by
    /// [`protected_status`]; it must never read as "protected".
    EntryPending,
}

impl Protection {
    /// Short text for the account panel and banners. `Undetermined` says so plainly: it must
    /// never read as "protected".
    pub fn label(self) -> &'static str {
        match self {
            Protection::Stop => "Stop-loss in place",
            Protection::PartialStop => "Stop-loss size unknown or smaller than the position",
            Protection::TrailingOnly => "Trailing stop only; may not be active yet",
            Protection::TargetOnly => "Take-profit only, no stop-loss",
            Protection::Unprotected => "No stop-loss",
            Protection::Undetermined => "Cannot tell: order and position lists may be out of date",
            Protection::EntryPending => {
                "Protection not active yet (entry protection pending, not in force yet)"
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PositionProtection {
    pub symbol: String,
    pub dir: Dir,
    pub hedge_side: bool,
    pub protection: Protection,
}

/// The position leg an order protects, when the row is a protective order at all: a
/// conditional order that can only reduce (`closePosition`, `reduceOnly`, or a hedge-mode
/// order on the closing side of its leg). One-way entry stops are not protective.
fn target_of(o: &OpenOrder) -> Option<(Dir, bool)> {
    o.role?;
    let side = o.side?;
    match o.position_side {
        Some(leg) => (side == leg.closing_side()).then_some((leg, true)),
        None => {
            if !(o.close_position || o.reduce_only) {
                return None;
            }
            let dir = match side {
                Side::Sell => Dir::Long,
                Side::Buy => Dir::Short,
            };
            Some((dir, false))
        }
    }
}

fn protects(pos: &Position, o: &OpenOrder) -> bool {
    o.symbol == pos.symbol && target_of(o) == Some((pos.dir, pos.hedge_side))
}

/// The held position a protective order with a stop (stop-loss, or stop and target in one) is
/// protecting, if any. Cancelling such an order changes that position's protection.
pub fn stop_guarded_position<'a>(positions: &'a [Position], o: &OpenOrder) -> Option<&'a Position> {
    if !o.role.is_some_and(ProtectRole::has_stop) {
        return None;
    }
    positions
        .iter()
        .find(|p| !p.qty.is_zero() && protects(p, o))
}

/// The open orders that protect `pos` (any protective row aimed at its leg), for display. The
/// caller must not show them when the lists may be stale.
pub fn protecting_orders<'a>(pos: &Position, orders: &'a [OpenOrder]) -> Vec<&'a OpenOrder> {
    orders.iter().filter(|o| protects(pos, o)).collect()
}

/// The protection of every held position. `lists_stale` (the stream is not synchronised, or an
/// algo-order event arrived after the snapshot) makes every answer `Undetermined`.
pub fn protected_status(
    positions: &[Position],
    orders: &[OpenOrder],
    lists_stale: bool,
) -> Vec<PositionProtection> {
    positions
        .iter()
        .filter(|p| !p.qty.is_zero())
        .map(|p| PositionProtection {
            symbol: p.symbol.clone(),
            dir: p.dir,
            hedge_side: p.hedge_side,
            protection: if lists_stale {
                Protection::Undetermined
            } else {
                status_of(p, orders)
            },
        })
        .collect()
}

fn status_of(pos: &Position, orders: &[OpenOrder]) -> Protection {
    let covering: Vec<&OpenOrder> = orders.iter().filter(|o| protects(pos, o)).collect();
    let stops: Vec<&OpenOrder> = covering
        .iter()
        .copied()
        .filter(|o| o.role.is_some_and(ProtectRole::has_stop))
        .collect();
    if !stops.is_empty() {
        if stops.iter().any(|o| o.close_position) {
            return Protection::Stop;
        }
        let mut total = Decimal::ZERO;
        for o in &stops {
            match o.qty.map(|q| total.checked_add(&q)) {
                Some(Ok(t)) => total = t,
                // An unknown size or an overflow: it cannot be called a full stop.
                _ => return Protection::PartialStop,
            }
        }
        return match total.checked_cmp(&pos.qty) {
            Ok(Ordering::Less) | Err(_) => Protection::PartialStop,
            Ok(_) => Protection::Stop,
        };
    }
    if covering
        .iter()
        .any(|o| o.role == Some(ProtectRole::Trailing))
    {
        return Protection::TrailingOnly;
    }
    if covering
        .iter()
        .any(|o| o.role.is_some_and(ProtectRole::has_target))
    {
        return Protection::TargetOnly;
    }
    Protection::Unprotected
}

/// Protective orders with no position to protect (a hedge leg that closed while the other is
/// still held counts). `None` when the lists may be stale: orders cannot be called orphans then.
pub fn orphan_protectives<'a>(
    positions: &[Position],
    orders: &'a [OpenOrder],
    lists_stale: bool,
) -> Option<Vec<&'a OpenOrder>> {
    if lists_stale {
        return None;
    }
    Some(
        orders
            .iter()
            .filter(|o| target_of(o).is_some())
            .filter(|o| !positions.iter().any(|p| !p.qty.is_zero() && protects(p, o)))
            // A row with no readable leg (read as one-way) on a symbol where a hedge leg is held
            // may be that leg's stop: it cannot be called an orphan.
            .filter(|o| {
                !(o.position_side.is_none()
                    && positions
                        .iter()
                        .any(|p| !p.qty.is_zero() && p.hedge_side && p.symbol == o.symbol))
            })
            .collect(),
    )
}

/// Positions with no whole stop, as [`protected_status`] judged them: no stop at all, a stop
/// that does not cover the position, a trailing stop only, or take-profit orders only. A stale
/// verdict (`Undetermined`), a stop, and protection that is not in force yet (`EntryPending`)
/// are not in this list.
pub fn bare_positions(statuses: &[PositionProtection]) -> Vec<&PositionProtection> {
    statuses
        .iter()
        .filter(|s| {
            matches!(
                s.protection,
                Protection::Unprotected
                    | Protection::PartialStop
                    | Protection::TrailingOnly
                    | Protection::TargetOnly
            )
        })
        .collect()
}

/// Algo orders that carry an id shaped like one of this app's but that `known` does not own (no
/// entry of the book, no order it placed): they may be left over from an earlier session.
/// `None` when the lists may be stale. Rows with no readable client id are never listed.
pub fn residual_auto_orders<'a>(
    orders: &'a [OpenOrder],
    lists_stale: bool,
    known: &dyn Fn(&str) -> bool,
) -> Option<Vec<&'a OpenOrder>> {
    if lists_stale {
        return None;
    }
    Some(
        orders
            .iter()
            .filter(|o| matches!(o.kind, super::client::OpenKind::Algo(_)))
            .filter(|o| {
                o.client_id
                    .as_deref()
                    .is_some_and(|id| super::client_id::looks_like_app_id(id) && !known(id))
            })
            .collect(),
    )
}

/// Can the order be cancelled by its id with the builders this build has? OKX algo orders need
/// their type, which a row may lack; a Binance order list is not cancelled here.
pub fn cancellable_by_id(o: &OpenOrder, okx: bool) -> bool {
    use super::client::OpenKind;
    match &o.kind {
        OpenKind::Plain => true,
        OpenKind::Algo(t) => !okx || t.is_some(),
        OpenKind::List => false,
    }
}

/// One line about a protective order for the account panel: what it is, where it triggers and
/// how much it covers. A field the row did not carry is said to be unreadable, never guessed.
/// `None` for an order that is not a protective one.
pub fn describe_protective(o: &OpenOrder) -> Option<String> {
    let role = o.role?;
    let size = if o.close_position {
        "whole position".to_string()
    } else {
        o.qty
            .map_or("size unreadable".to_string(), |q| format!("qty {q}"))
    };
    let trigger = o
        .trigger_price
        .map_or("trigger unreadable".to_string(), |t| {
            format!("triggers at {t}")
        });
    let after = if o.limit {
        match o.limit_price {
            Some(p) => format!(", then a limit order at {p} (it may not fill)"),
            None => ", then a limit order (it may not fill; price unreadable)".to_string(),
        }
    } else {
        String::new()
    };
    Some(match role {
        ProtectRole::Stop => format!("Stop-loss: {trigger}{after}, {size}"),
        ProtectRole::Target => format!("Take-profit: {trigger}{after}, {size}"),
        ProtectRole::StopAndTarget => {
            format!("Stop-loss and take-profit in one order: {trigger}{after}, {size}")
        }
        ProtectRole::Trailing => {
            let callback = o
                .callback_rate
                .map_or("callback unreadable".to_string(), |c| {
                    format!("callback {c}%")
                });
            let active = o.activation_price.map_or(
                "no activation price read, so it may not be active".to_string(),
                |a| format!("activates at {a}"),
            );
            format!("Trailing stop (not counted as a stop-loss): {callback}, {active}, {size}")
        }
    })
}

// ── Trigger prices and quantities from a fill ───────────────────────────

/// How far a trigger is from the entry fill price.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Offset {
    /// Percent of the fill price (`1.5` = 1.5 %).
    Percent(Decimal),
    /// A fixed price distance.
    Distance(Decimal),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TargetSpec {
    pub offset: Offset,
    /// Whole percent of the filled quantity; all targets sum to 100.
    pub share_pct: u8,
}

/// Which price a trigger watches. Neutral twin of the venue types, so `exchange` need not know
/// the saved-settings type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum TriggerBasis {
    #[default]
    Last,
    Mark,
}

/// A trailing stop: callback rate in percent, and where the trail starts (`None` = at once).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TrailingSpec {
    pub callback_pct: Decimal,
    pub activation: Option<Offset>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Hash)]
pub struct ProtectSpec {
    pub stop: Option<Offset>,
    pub targets: Vec<TargetSpec>,
    pub trailing: Option<TrailingSpec>,
    pub trigger_price: TriggerBasis,
}

/// A hash of everything a [`ProtectSpec`] decides (trigger basis, distances, shares, trailing
/// parameters). It is made from a spec only, so two equal fingerprints mean the same settings.
/// In memory only: never saved or sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpecFingerprint(u64);

impl ProtectSpec {
    /// The fingerprint of these settings. `DefaultHasher::new()` uses fixed keys, so the value
    /// is the same for the same settings within one run (which is all it is used for).
    pub fn fingerprint(&self) -> SpecFingerprint {
        use std::hash::{DefaultHasher, Hash, Hasher};
        let mut h = DefaultHasher::new();
        self.hash(&mut h);
        SpecFingerprint(h.finish())
    }

    /// Nothing to attach. A spec with only a trailing stop is not empty.
    pub fn is_empty(&self) -> bool {
        self.stop.is_none() && self.targets.is_empty() && self.trailing.is_none()
    }
}

/// Price tick and quantity step of the symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rounding {
    pub tick: Decimal,
    pub step: Decimal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TargetLeg {
    pub trigger: Decimal,
    pub qty: Decimal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtectPlan {
    pub stop: Option<Decimal>,
    /// Filled quantity rounded down to the step: the size a quantity-type stop would take.
    pub total_qty: Decimal,
    pub targets: Vec<TargetLeg>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanError {
    /// Fill price, filled quantity, tick or step is not positive.
    BadInput,
    /// An offset is not positive.
    BadOffset,
    /// More than three targets, a share outside 1..=100, or shares that do not sum to 100.
    BadShares,
    /// A trigger would not be strictly on its side of the fill price (or not positive) once
    /// rounded to the tick.
    WrongSide,
    /// A target would get no quantity at this step.
    QtyTooSmall,
    Math(DecimalError),
}

impl From<DecimalError> for PlanError {
    fn from(e: DecimalError) -> Self {
        PlanError::Math(e)
    }
}

pub const MAX_TARGETS: usize = 3;

fn delta(fill: &Decimal, offset: Offset) -> Result<Decimal, PlanError> {
    let (raw, d) = match offset {
        Offset::Distance(d) => (d, d),
        Offset::Percent(p) => (
            p,
            fill.checked_mul(&p)?
                .checked_mul(&Decimal::parse("0.01")?)?,
        ),
    };
    if !raw.is_positive() || !d.is_positive() {
        return Err(PlanError::BadOffset);
    }
    Ok(d)
}

/// One trigger, rounded to the tick **towards the entry price** (never wider than asked), and
/// required to stay strictly on its side of the fill.
fn trigger(
    dir: Dir,
    fill: &Decimal,
    offset: Offset,
    tick: &Decimal,
    stop: bool,
) -> Result<Decimal, PlanError> {
    let d = delta(fill, offset)?;
    // A stop is against the position, a target is with it.
    let below = (dir == Dir::Long) == stop;
    let raw = if below {
        fill.checked_sub(&d)?
    } else {
        fill.checked_add(&d)?
    };
    let rounded = if below {
        raw.ceil_to_step(tick)?
    } else {
        raw.floor_to_step(tick)?
    };
    let want = if below {
        Ordering::Less
    } else {
        Ordering::Greater
    };
    if !rounded.is_positive() || rounded.checked_cmp(fill)? != want {
        return Err(PlanError::WrongSide);
    }
    Ok(rounded)
}

/// The trigger of a stop for a position of `dir` at the price `base`: `base` less the offset
/// (more, for a short). Not rounded to the tick: the order builder rounds it and checks that it
/// is on the right side of the market. Used to offer a stop again for a position that is
/// already open, where the base is the current price. Pure: nothing is sent.
pub fn stop_trigger_at(dir: Dir, base: &Decimal, offset: Offset) -> Result<Decimal, PlanError> {
    if !base.is_positive() {
        return Err(PlanError::BadInput);
    }
    let d = delta(base, offset)?;
    let raw = match dir {
        Dir::Long => base.checked_sub(&d)?,
        Dir::Short => base.checked_add(&d)?,
    };
    if raw.is_positive() {
        Ok(raw)
    } else {
        Err(PlanError::WrongSide)
    }
}

/// The trigger prices and target quantities for a fill. Pure: nothing is sent.
pub fn plan(
    dir: Dir,
    fill_price: Decimal,
    filled_qty: Decimal,
    spec: &ProtectSpec,
    rounding: &Rounding,
) -> Result<ProtectPlan, PlanError> {
    if !fill_price.is_positive()
        || !filled_qty.is_positive()
        || !rounding.tick.is_positive()
        || !rounding.step.is_positive()
    {
        return Err(PlanError::BadInput);
    }
    let shares: u32 = spec.targets.iter().map(|t| u32::from(t.share_pct)).sum();
    if spec.targets.len() > MAX_TARGETS
        || spec
            .targets
            .iter()
            .any(|t| !(1..=100).contains(&t.share_pct))
        || (!spec.targets.is_empty() && shares != 100)
    {
        return Err(PlanError::BadShares);
    }
    let total = filled_qty.trunc_to_step(&rounding.step)?;
    if total.is_zero() && !spec.is_empty() {
        return Err(PlanError::QtyTooSmall);
    }
    let stop = spec
        .stop
        .map(|o| trigger(dir, &fill_price, o, &rounding.tick, true))
        .transpose()?;
    let hundredth = Decimal::parse("0.01")?;
    let mut targets = Vec::with_capacity(spec.targets.len());
    let mut used = Decimal::ZERO;
    for (i, t) in spec.targets.iter().enumerate() {
        let qty = if i + 1 == spec.targets.len() {
            // The last target takes what is left, so rounding leaves no remainder.
            total.checked_sub(&used)?
        } else {
            total
                .checked_mul(&Decimal::from_int(i64::from(t.share_pct)))?
                .checked_mul(&hundredth)?
                .trunc_to_step(&rounding.step)?
        };
        if !qty.is_positive() {
            return Err(PlanError::QtyTooSmall);
        }
        used = used.checked_add(&qty)?;
        targets.push(TargetLeg {
            trigger: trigger(dir, &fill_price, t.offset, &rounding.tick, false)?,
            qty,
        });
    }
    Ok(ProtectPlan {
        stop,
        total_qty: total,
        targets,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trade::client::OpenKind;

    #[test]
    fn a_stop_trigger_for_an_open_position_is_on_the_losing_side_of_the_base() {
        let base = d("50000");
        assert_eq!(
            stop_trigger_at(Dir::Long, &base, Offset::Percent(d("2"))),
            Ok(d("49000"))
        );
        assert_eq!(
            stop_trigger_at(Dir::Short, &base, Offset::Percent(d("2"))),
            Ok(d("51000"))
        );
        assert_eq!(
            stop_trigger_at(Dir::Long, &base, Offset::Distance(d("250"))),
            Ok(d("49750"))
        );
        // A distance that reaches zero or below, a zero offset and a bad base are refused.
        assert_eq!(
            stop_trigger_at(Dir::Long, &base, Offset::Distance(d("50000"))),
            Err(PlanError::WrongSide)
        );
        assert_eq!(
            stop_trigger_at(Dir::Long, &base, Offset::Percent(d("0"))),
            Err(PlanError::BadOffset)
        );
        assert_eq!(
            stop_trigger_at(Dir::Long, &d("0"), Offset::Percent(d("1"))),
            Err(PlanError::BadInput)
        );
    }

    fn d(s: &str) -> Decimal {
        Decimal::parse(s).expect("decimal")
    }

    fn pos(symbol: &str, dir: Dir, hedge: bool, qty: &str) -> Position {
        Position {
            symbol: symbol.into(),
            dir,
            qty: d(qty),
            hedge_side: hedge,
            margin: None,
            leverage: None,
        }
    }

    struct O(OpenOrder);

    fn order(symbol: &str, role: ProtectRole, side: Side) -> O {
        O(OpenOrder {
            symbol: symbol.into(),
            id: "1".into(),
            kind: OpenKind::Algo(None),
            side: Some(side),
            position_side: None,
            qty: None,
            trigger_price: None,
            close_position: false,
            reduce_only: false,
            role: Some(role),
            attached: None,
            client_id: None,
            limit: false,
            limit_price: None,
            activation_price: None,
            callback_rate: None,
        })
    }

    impl O {
        fn close(mut self) -> Self {
            self.0.close_position = true;
            self
        }
        fn reduce(mut self) -> Self {
            self.0.reduce_only = true;
            self
        }
        fn qty(mut self, q: &str) -> Self {
            self.0.qty = Some(d(q));
            self
        }
        fn leg(mut self, dir: Dir) -> Self {
            self.0.position_side = Some(dir);
            self
        }
    }

    fn status(positions: &[Position], orders: Vec<O>, stale: bool) -> Vec<Protection> {
        let orders: Vec<OpenOrder> = orders.into_iter().map(|o| o.0).collect();
        protected_status(positions, &orders, stale)
            .into_iter()
            .map(|p| p.protection)
            .collect()
    }

    #[test]
    fn one_way_status_table() {
        let long = [pos("BTCUSDT", Dir::Long, false, "2")];
        let sl = || order("BTCUSDT", ProtectRole::Stop, Side::Sell);
        let tp = || order("BTCUSDT", ProtectRole::Target, Side::Sell);
        let cases: Vec<(Vec<O>, Protection)> = vec![
            (vec![], Protection::Unprotected),
            (vec![sl().close()], Protection::Stop),
            (vec![sl().reduce().qty("2")], Protection::Stop),
            (vec![sl().reduce().qty("1")], Protection::PartialStop),
            (
                vec![sl().reduce().qty("1"), sl().reduce().qty("1")],
                Protection::Stop,
            ),
            // A stop with no known size cannot be called a full stop.
            (vec![sl().reduce()], Protection::PartialStop),
            (vec![tp().close()], Protection::TargetOnly),
            (vec![tp().close(), sl().close()], Protection::Stop),
            // Not reducing: a one-way entry stop is not protection.
            (vec![sl()], Protection::Unprotected),
            // Wrong side: a buy stop does not protect a long.
            (
                vec![order("BTCUSDT", ProtectRole::Stop, Side::Buy).close()],
                Protection::Unprotected,
            ),
            (
                vec![order("ETHUSDT", ProtectRole::Stop, Side::Sell).close()],
                Protection::Unprotected,
            ),
            // A trailing stop may not be active yet (activation price): never `Stop`.
            (
                vec![order("BTCUSDT", ProtectRole::Trailing, Side::Sell).close()],
                Protection::TrailingOnly,
            ),
            (
                vec![
                    order("BTCUSDT", ProtectRole::Trailing, Side::Sell)
                        .reduce()
                        .qty("5"),
                ],
                Protection::TrailingOnly,
            ),
            // Trailing does not add to the size a real stop covers.
            (
                vec![
                    sl().reduce().qty("1"),
                    order("BTCUSDT", ProtectRole::Trailing, Side::Sell)
                        .reduce()
                        .qty("1"),
                ],
                Protection::PartialStop,
            ),
            (
                vec![
                    sl().close(),
                    order("BTCUSDT", ProtectRole::Trailing, Side::Sell).close(),
                ],
                Protection::Stop,
            ),
            (
                vec![order("BTCUSDT", ProtectRole::StopAndTarget, Side::Sell).close()],
                Protection::Stop,
            ),
        ];
        for (i, (orders, want)) in cases.into_iter().enumerate() {
            assert_eq!(status(&long, orders, false), vec![want], "case {i}");
        }
        let short = [pos("BTCUSDT", Dir::Short, false, "1")];
        assert_eq!(
            status(
                &short,
                vec![order("BTCUSDT", ProtectRole::Stop, Side::Buy).close()],
                false
            ),
            vec![Protection::Stop]
        );
    }

    #[test]
    fn hedge_legs_are_judged_apart() {
        let both = [
            pos("BTCUSDT", Dir::Long, true, "1"),
            pos("BTCUSDT", Dir::Short, true, "1"),
        ];
        let long_stop = order("BTCUSDT", ProtectRole::Stop, Side::Sell)
            .close()
            .leg(Dir::Long);
        assert_eq!(
            status(&both, vec![long_stop], false),
            vec![Protection::Stop, Protection::Unprotected]
        );
        // Same side as the entry on its own leg: that is an entry stop.
        let entry_stop = order("BTCUSDT", ProtectRole::Stop, Side::Buy)
            .close()
            .leg(Dir::Long);
        assert_eq!(
            status(&both, vec![entry_stop], false),
            vec![Protection::Unprotected, Protection::Unprotected]
        );
        // A one-way style order does not protect a hedge leg.
        let oneway = order("BTCUSDT", ProtectRole::Stop, Side::Sell).close();
        assert_eq!(
            status(&both, vec![oneway], false),
            vec![Protection::Unprotected, Protection::Unprotected]
        );
    }

    #[test]
    fn stale_lists_are_undetermined_never_protected() {
        let long = [pos("BTCUSDT", Dir::Long, false, "2")];
        let stop = order("BTCUSDT", ProtectRole::Stop, Side::Sell).close();
        assert_eq!(
            status(&long, vec![stop], true),
            vec![Protection::Undetermined]
        );
        assert!(Protection::Undetermined.label().starts_with("Cannot tell"));
        // The same positions with fresh lists and no orders are plainly unprotected.
        assert_eq!(status(&long, vec![], false), vec![Protection::Unprotected]);
        // A flat (zero) row is not a position.
        assert!(status(&[pos("BTCUSDT", Dir::Long, false, "0")], vec![], false).is_empty());
    }

    #[test]
    fn orphans_are_protective_orders_without_a_position() {
        let ids = |positions: &[Position], orders: Vec<O>, stale| {
            let orders: Vec<OpenOrder> = orders.into_iter().map(|o| o.0).collect();
            orphan_protectives(positions, &orders, stale)
                .map(|v| v.iter().map(|o| o.symbol.clone()).collect::<Vec<_>>())
        };
        let stop = |s: &str| order(s, ProtectRole::Stop, Side::Sell).close();
        // No position at all.
        assert_eq!(
            ids(&[], vec![stop("BTCUSDT")], false),
            Some(vec!["BTCUSDT".to_string()])
        );
        // Held, so not an orphan.
        let held = [pos("BTCUSDT", Dir::Long, false, "1")];
        assert_eq!(ids(&held, vec![stop("BTCUSDT")], false), Some(vec![]));
        // The position is short: a sell stop has nothing to close.
        let short = [pos("BTCUSDT", Dir::Short, false, "1")];
        assert_eq!(
            ids(&short, vec![stop("BTCUSDT")], false).map(|v| v.len()),
            Some(1)
        );
        // Hedge: the short leg closed, the long leg is still held.
        let long_leg = [pos("BTCUSDT", Dir::Long, true, "1")];
        let long_sl = order("BTCUSDT", ProtectRole::Stop, Side::Sell)
            .close()
            .leg(Dir::Long);
        let short_sl = order("BTCUSDT", ProtectRole::Stop, Side::Buy)
            .close()
            .leg(Dir::Short);
        assert_eq!(
            ids(&long_leg, vec![long_sl, short_sl], false),
            Some(vec!["BTCUSDT".to_string()])
        );
        // An entry order is not a protective one, so never an orphan.
        let entry = order("BTCUSDT", ProtectRole::Stop, Side::Buy);
        assert_eq!(ids(&[], vec![entry], false), Some(vec![]));
        // Stale lists: no verdict.
        assert_eq!(ids(&[], vec![stop("BTCUSDT")], true), None);
    }

    fn spec(stop: Option<Offset>, targets: &[(Offset, u8)]) -> ProtectSpec {
        ProtectSpec {
            stop,
            targets: targets
                .iter()
                .map(|&(offset, share_pct)| TargetSpec { offset, share_pct })
                .collect(),
            ..ProtectSpec::default()
        }
    }

    fn rounding() -> Rounding {
        Rounding {
            tick: d("0.1"),
            step: d("0.001"),
        }
    }

    fn pct(s: &str) -> Offset {
        Offset::Percent(d(s))
    }

    fn dist(s: &str) -> Offset {
        Offset::Distance(d(s))
    }

    #[test]
    fn plan_long_and_short_by_percent_and_distance() {
        let long = plan(
            Dir::Long,
            d("100"),
            d("1"),
            &spec(Some(pct("1")), &[(dist("2.55"), 100)]),
            &rounding(),
        )
        .expect("plan");
        assert_eq!(long.stop, Some(d("99")));
        assert_eq!(
            long.targets,
            vec![TargetLeg {
                trigger: d("102.5"),
                qty: d("1")
            }]
        );
        let short = plan(
            Dir::Short,
            d("100"),
            d("1"),
            &spec(Some(dist("1.55")), &[(pct("2.55"), 100)]),
            &rounding(),
        )
        .expect("plan");
        // Rounded towards the entry price: the stop never wider, the target never farther.
        assert_eq!(short.stop, Some(d("101.5")));
        assert_eq!(short.targets[0].trigger, d("97.5"));
    }

    #[test]
    fn plan_rounds_quantities_and_the_last_target_takes_the_rest() {
        let p = plan(
            Dir::Long,
            d("100"),
            d("1.0007"),
            &spec(None, &[(pct("1"), 33), (pct("2"), 33), (pct("3"), 34)]),
            &rounding(),
        )
        .expect("plan");
        assert_eq!(p.total_qty, d("1"));
        assert_eq!(p.targets[0].qty, d("0.33"));
        assert_eq!(p.targets[1].qty, d("0.33"));
        assert_eq!(p.targets[2].qty, d("0.34"));
        let sum = p
            .targets
            .iter()
            .fold(Decimal::ZERO, |a, t| a.checked_add(&t.qty).expect("sum"));
        assert_eq!(sum, p.total_qty);
        assert_eq!(p.stop, None);
    }

    #[test]
    fn plan_refuses_what_cannot_be_trusted() {
        let r = rounding();
        let one = |s| plan(Dir::Long, d("100"), d("1"), &s, &r);
        // Shares that do not sum to 100, a fourth leg, a zero share.
        assert_eq!(
            one(spec(None, &[(pct("1"), 50)])),
            Err(PlanError::BadShares)
        );
        assert_eq!(
            one(spec(
                None,
                &[
                    (pct("1"), 25),
                    (pct("2"), 25),
                    (pct("3"), 25),
                    (pct("4"), 25)
                ]
            )),
            Err(PlanError::BadShares)
        );
        assert_eq!(
            one(spec(None, &[(pct("1"), 0), (pct("2"), 100)])),
            Err(PlanError::BadShares)
        );
        // Non-positive offsets.
        assert_eq!(one(spec(Some(pct("0")), &[])), Err(PlanError::BadOffset));
        assert_eq!(one(spec(Some(dist("-1")), &[])), Err(PlanError::BadOffset));
        // A stop at or below zero, and one that rounds onto the fill price.
        assert_eq!(one(spec(Some(pct("100")), &[])), Err(PlanError::WrongSide));
        assert_eq!(
            one(spec(Some(dist("0.05")), &[])),
            Err(PlanError::WrongSide)
        );
        // A leg too small for the step.
        let tiny = plan(
            Dir::Long,
            d("100"),
            d("0.002"),
            &spec(None, &[(pct("1"), 10), (pct("2"), 90)]),
            &r,
        );
        assert_eq!(tiny, Err(PlanError::QtyTooSmall));
        // Bad inputs.
        assert_eq!(
            plan(Dir::Long, d("0"), d("1"), &ProtectSpec::default(), &r),
            Err(PlanError::BadInput)
        );
        assert_eq!(
            plan(
                Dir::Long,
                d("100"),
                d("0.0004"),
                &spec(Some(pct("1")), &[]),
                &r
            ),
            Err(PlanError::QtyTooSmall)
        );
    }

    // ── d5: bare positions, residual auto orders, labels ──────────────────

    fn plain(symbol: &str, id: &str) -> OpenOrder {
        let mut o = order("X", ProtectRole::Stop, Side::Sell).0;
        o.symbol = symbol.into();
        o.kind = OpenKind::Plain;
        o.role = None;
        o.client_id = Some(id.into());
        o
    }

    fn algo_with_id(id: Option<&str>) -> OpenOrder {
        let mut o = order("BTCUSDT", ProtectRole::Stop, Side::Sell).close().0;
        o.client_id = id.map(str::to_string);
        o
    }

    #[test]
    fn bare_positions_are_the_ones_without_a_whole_stop() {
        let positions = [
            pos("AAAUSDT", Dir::Long, false, "1"),
            pos("BBBUSDT", Dir::Long, false, "1"),
            pos("CCCUSDT", Dir::Long, false, "2"),
            pos("DDDUSDT", Dir::Long, false, "1"),
        ];
        let mut stop = order("BBBUSDT", ProtectRole::Stop, Side::Sell).close().0;
        stop.trigger_price = Some(d("1"));
        let mut part = order("CCCUSDT", ProtectRole::Stop, Side::Sell)
            .reduce()
            .qty("1")
            .0;
        part.trigger_price = Some(d("1"));
        let mut tp = order("DDDUSDT", ProtectRole::Target, Side::Sell).close().0;
        tp.trigger_price = Some(d("1"));
        let orders = [stop, part, tp];
        let all = protected_status(&positions, &orders, false);
        let bare: Vec<(&str, Protection)> = bare_positions(&all)
            .into_iter()
            .map(|b| (b.symbol.as_str(), b.protection))
            .collect();
        assert_eq!(
            bare,
            vec![
                ("AAAUSDT", Protection::Unprotected),
                ("CCCUSDT", Protection::PartialStop),
                ("DDDUSDT", Protection::TargetOnly),
            ]
        );
    }

    #[test]
    fn a_stale_list_has_no_bare_positions_and_no_orphans() {
        let positions = [pos("BTCUSDT", Dir::Long, false, "1")];
        let all = protected_status(&positions, &[], true);
        assert!(bare_positions(&all).is_empty(), "cannot tell is not bare");
        let stray = algo_with_id(Some("sAAAAAAAAAAAAAAAAAAAA"));
        assert!(orphan_protectives(&[], std::slice::from_ref(&stray), true).is_none());
        assert!(residual_auto_orders(std::slice::from_ref(&stray), true, &|_| false).is_none());
    }

    #[test]
    fn hedge_legs_are_judged_one_by_one() {
        let positions = [
            pos("BTCUSDT", Dir::Long, true, "1"),
            pos("BTCUSDT", Dir::Short, true, "1"),
        ];
        let mut long_stop = order("BTCUSDT", ProtectRole::Stop, Side::Sell)
            .leg(Dir::Long)
            .close()
            .0;
        long_stop.trigger_price = Some(d("1"));
        let all = protected_status(&positions, std::slice::from_ref(&long_stop), false);
        let bare = bare_positions(&all);
        assert_eq!(bare.len(), 1);
        assert_eq!(bare[0].dir, Dir::Short);
        // The short leg closes, the long stop is still there: that stop has a position.
        let only_long = [pos("BTCUSDT", Dir::Long, true, "1")];
        assert!(
            orphan_protectives(&only_long, std::slice::from_ref(&long_stop), false)
                .unwrap()
                .is_empty()
        );
        let only_short = [pos("BTCUSDT", Dir::Short, true, "1")];
        assert_eq!(
            orphan_protectives(&only_short, std::slice::from_ref(&long_stop), false)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn an_unreadable_position_side_neither_protects_a_hedge_leg_nor_is_an_orphan() {
        // A hedge account whose order row has no readable `positionSide` is read as one-way
        // (no leg): it does not protect a hedge leg, and it must not be called an orphan either.
        let hedge = [pos("BTCUSDT", Dir::Long, true, "1")];
        let mut unreadable = order("BTCUSDT", ProtectRole::Stop, Side::Sell).close().0;
        unreadable.trigger_price = Some(d("1"));
        unreadable.position_side = None;
        let all = protected_status(&hedge, std::slice::from_ref(&unreadable), false);
        assert_eq!(all[0].protection, Protection::Unprotected);
        // It may still be that leg's stop: it is not offered for cancelling.
        let orphans = orphan_protectives(&hedge, std::slice::from_ref(&unreadable), false).unwrap();
        assert!(orphans.is_empty());
        // With no hedge leg held it is an orphan as before.
        let none = orphan_protectives(&[], std::slice::from_ref(&unreadable), false).unwrap();
        assert_eq!(none.len(), 1);
    }

    #[test]
    fn residual_auto_orders_need_an_app_shaped_id_that_nothing_owns() {
        let mine = "sAAAAAAAAAAAAAAAAAAAA";
        let other = "tBBBBBBBBBBBBBBBBBBBB";
        let orders = vec![
            algo_with_id(Some(mine)),
            algo_with_id(Some(other)),
            algo_with_id(Some("web_stop_1")),
            algo_with_id(None),
            plain("BTCUSDT", "sCCCCCCCCCCCCCCCCCCCC"),
        ];
        let got = residual_auto_orders(&orders, false, &|id| id == mine).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].client_id.as_deref(), Some(other));
    }

    #[test]
    fn the_app_id_shape_is_a_role_letter_and_twenty_base62_characters() {
        assert!(crate::trade::looks_like_app_id("eAAAAAAAAAAAAAAAAAAAA"));
        assert!(crate::trade::looks_like_app_id("r0123456789abcdefghij"));
        assert!(!crate::trade::looks_like_app_id("xAAAAAAAAAAAAAAAAAAAA"));
        assert!(!crate::trade::looks_like_app_id("eAAAA"));
        assert!(!crate::trade::looks_like_app_id("e-AAAAAAAAAAAAAAAAAAA"));
        assert!(!crate::trade::looks_like_app_id(""));
    }

    #[test]
    fn a_debug_print_of_an_open_order_hides_the_client_id() {
        let o = algo_with_id(Some("sSECRETSECRETSECRETSE"));
        assert!(!format!("{o:?}").contains("SECRET"));
    }

    #[test]
    fn what_can_be_cancelled_by_id() {
        let algo_none = algo_with_id(None);
        assert!(cancellable_by_id(&algo_none, false));
        assert!(
            !cancellable_by_id(&algo_none, true),
            "an OKX algo needs its type"
        );
        let mut typed = algo_none.clone();
        typed.kind = OpenKind::Algo(Some(crate::trade::okx::OkxAlgoType::Conditional));
        assert!(cancellable_by_id(&typed, true));
        let mut list = algo_none;
        list.kind = OpenKind::List;
        assert!(!cancellable_by_id(&list, false));
    }

    #[test]
    fn every_protective_role_has_a_line_and_unreadable_fields_are_said() {
        let mut stop = order("BTCUSDT", ProtectRole::Stop, Side::Sell).close().0;
        stop.trigger_price = Some(d("59000"));
        assert_eq!(
            describe_protective(&stop).unwrap(),
            "Stop-loss: triggers at 59000, whole position"
        );
        stop.close_position = false;
        assert!(
            describe_protective(&stop)
                .unwrap()
                .ends_with("size unreadable")
        );
        stop.limit = true;
        assert!(
            describe_protective(&stop)
                .unwrap()
                .contains("limit order (it may not fill; price unreadable)")
        );
        stop.limit_price = Some(d("58900"));
        assert!(
            describe_protective(&stop)
                .unwrap()
                .contains("limit order at 58900 (it may not fill)")
        );
        let mut tp = order("BTCUSDT", ProtectRole::Target, Side::Sell)
            .reduce()
            .qty("0.5")
            .0;
        tp.trigger_price = Some(d("61000"));
        assert_eq!(
            describe_protective(&tp).unwrap(),
            "Take-profit: triggers at 61000, qty 0.5"
        );
        let mut both = order("BTCUSDT", ProtectRole::StopAndTarget, Side::Sell)
            .close()
            .0;
        both.trigger_price = Some(d("59000"));
        assert!(
            describe_protective(&both)
                .unwrap()
                .starts_with("Stop-loss and take-profit")
        );
        let mut trail = order("BTCUSDT", ProtectRole::Trailing, Side::Sell)
            .reduce()
            .qty("1")
            .0;
        let line = describe_protective(&trail).unwrap();
        assert!(line.contains("not counted as a stop-loss"));
        assert!(line.contains("callback unreadable"));
        assert!(line.contains("may not be active"));
        trail.callback_rate = Some(d("1.5"));
        trail.activation_price = Some(d("60000"));
        let line = describe_protective(&trail).unwrap();
        assert!(line.contains("callback 1.5%") && line.contains("activates at 60000"));
        let mut not_protective = stop.clone();
        not_protective.role = None;
        assert!(describe_protective(&not_protective).is_none());
    }
}
