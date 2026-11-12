//! What the charts and the order book draw of an account's orders and position: a read-only
//! view model made from the account's last lists. It is never saved (no `Serialize`) and is not
//! part of the drawings library (conditions 51 and 78 in `docs/trading-security.md`). Since f3a2
//! a stop or target line that can be dragged carries the exchange id of its order (`Grab::Yes`),
//! so a drag names the order it moves; that id is memory-only like the rest of the line, and it is
//! never a client id.

use exchange::trade::{Decimal, Dir, OpenKind, OpenOrder, ProtectRole, Side};

use crate::replace_flow::DragBlock;
use crate::trading::HeldPosition;

/// Whether the line may be dragged to another price (f3a2).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Grab {
    /// Not a stop or a target of the replace path (entry, trailing, position, plain limit…).
    #[default]
    No,
    /// A stop or target that cannot be dragged now, with the reason the chart shows.
    Blocked(DragBlock),
    /// Draggable; the exchange id of the order the line is drawn from.
    Yes(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// An order that opens (or adds to) a position: a conditional one drawn at its trigger, a
    /// plain limit one at its limit price.
    Entry,
    Stop,
    Target,
    Trailing,
    /// The average entry price of a held position.
    Position,
}

impl LineKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Entry => "Entry",
            Self::Stop => "SL",
            Self::Target => "TP",
            Self::Trailing => "Trail",
            Self::Position => "Pos",
        }
    }
}

/// One line on a price axis. `Clone`, not `Serialize`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderLine {
    pub price: Decimal,
    pub kind: LineKind,
    pub side: Side,
    /// `None`: the row did not carry a size, or it closes the whole position (`whole`).
    pub qty: Option<Decimal>,
    pub whole: bool,
    /// The lists may not match the exchange (condition 51): not connected, not synchronised or
    /// an algo event arrived after the snapshot. The line is drawn as "may be out of date".
    pub stale: bool,
    /// Draggable, blocked or neither (f3a2).
    pub grab: Grab,
    /// The size is in OKX contracts, not base units: the label and the tooltip say so.
    pub contracts: bool,
}

impl OrderLine {
    /// The text on the price axis: the kind, the size and, when the lists may be out of date,
    /// "stale".
    pub fn label(&self) -> String {
        let mut text = self.kind.label().to_string();
        if self.whole {
            text.push_str(" all");
        } else if let Some(q) = &self.qty {
            text.push(' ');
            text.push_str(&q.to_plain_string());
            if self.contracts {
                text.push_str(" ct");
            }
        }
        if self.kind == LineKind::Trailing {
            text.push_str(" trailing");
        }
        if self.stale {
            text.push_str(" · stale");
        }
        text
    }

    /// The price in the units of the order book's `Price` (11 decimals), exact, so a price that
    /// sits on a row's step is not moved to the next row by float noise. `None` when it does not
    /// fit.
    pub fn price_units(&self) -> Option<i64> {
        const SCALE_DIGITS: usize = 11;
        let text = self.price.to_plain_string();
        let (int, frac) = text.split_once('.').unwrap_or((text.as_str(), ""));
        let frac = &frac[..frac.len().min(SCALE_DIGITS)];
        let int: i64 = int.parse().ok()?;
        let frac: i64 = format!("{frac:0<SCALE_DIGITS$}").parse().ok()?;
        int.checked_mul(10_i64.pow(SCALE_DIGITS as u32))?
            .checked_add(frac)
    }

    /// The price as a float for a chart axis; `None` when it does not fit one.
    pub fn price_f32(&self) -> Option<f32> {
        self.price
            .to_plain_string()
            .parse::<f32>()
            .ok()
            .filter(|p| p.is_finite() && *p > 0.0)
    }
}

/// A line with the exchange symbol it belongs to (the pane filters on it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolLine {
    pub symbol: String,
    pub line: OrderLine,
}

/// What the lines of one account need to know besides its rows (f3a2).
pub struct Ctx<'a> {
    /// A spot account: a sell of what is held reduces, there are no reduce-only flags.
    pub spot: bool,
    /// OKX swap: sizes are contracts.
    pub contracts: bool,
    /// Whether a row's line may be dragged; called for stop and target rows only.
    pub grab: &'a dyn Fn(&OpenOrder) -> Grab,
}

/// The lines of one account, from its open orders and held positions. A row that lacks a field
/// (side, price) is not drawn: nothing is guessed. Nothing is draggable here.
#[cfg(test)]
pub fn lines_of(orders: &[OpenOrder], positions: &[HeldPosition], stale: bool) -> Vec<SymbolLine> {
    lines_with(
        orders,
        positions,
        stale,
        &Ctx {
            spot: false,
            contracts: false,
            grab: &|_| Grab::No,
        },
    )
}

/// [`lines_of`] with the account's context: spot sells, contract sizes and which lines can be
/// grabbed.
pub fn lines_with(
    orders: &[OpenOrder],
    positions: &[HeldPosition],
    stale: bool,
    ctx: &Ctx<'_>,
) -> Vec<SymbolLine> {
    let mut out: Vec<SymbolLine> = orders
        .iter()
        .filter_map(|o| order_line(o, stale, ctx))
        .collect();
    out.extend(
        positions
            .iter()
            .filter_map(|p| position_line(p, stale, ctx)),
    );
    out
}

fn positive(d: Option<Decimal>) -> Option<Decimal> {
    d.filter(Decimal::is_positive)
}

fn order_line(o: &OpenOrder, stale: bool, ctx: &Ctx<'_>) -> Option<SymbolLine> {
    // A plain resting limit order has no role. It is an entry unless it reduces the position,
    // and a reducing limit (a manual take-profit style close) is drawn as a target: it exits
    // at a chosen price, which is what a target line says. Nothing else is guessed.
    let Some(role) = o.role else {
        return plain_limit_line(o, stale, ctx);
    };
    // Only a conditional (algo) row has a role. An OKX entry that merely carries attached
    // take-profit / stop-loss has none (`attached`): they are not in force, so nothing is drawn.
    let side = o.side?;
    let trigger = if role == ProtectRole::Trailing {
        o.trigger_price.or(o.activation_price)
    } else {
        o.trigger_price
    };
    let price = positive(trigger)?;
    let reduces = match o.position_side {
        Some(leg) => side == leg.closing_side(),
        None => o.close_position || o.reduce_only,
    };
    let kind = match (reduces, role) {
        (false, _) => LineKind::Entry,
        (true, ProtectRole::Stop | ProtectRole::StopAndTarget) => LineKind::Stop,
        (true, ProtectRole::Target) => LineKind::Target,
        (true, ProtectRole::Trailing) => LineKind::Trailing,
    };
    // Only a stop or a target of one trigger can be moved (the exchange layer decides again).
    let grab = match (kind, role) {
        (LineKind::Stop, ProtectRole::Stop) | (LineKind::Target, ProtectRole::Target) => {
            (ctx.grab)(o)
        }
        _ => Grab::No,
    };
    Some(SymbolLine {
        symbol: o.symbol.clone(),
        line: OrderLine {
            price,
            kind,
            side,
            qty: if o.close_position { None } else { o.qty },
            whole: o.close_position,
            stale,
            grab,
            contracts: ctx.contracts,
        },
    })
}

fn plain_limit_line(o: &OpenOrder, stale: bool, ctx: &Ctx<'_>) -> Option<SymbolLine> {
    if o.kind != OpenKind::Plain {
        return None;
    }
    let price = positive(o.price)?;
    let side = o.side?;
    // A spot sell is a sale of what is held: it never opens anything, so it is a target (an exit
    // at a chosen price), not an entry. A spot buy adds.
    let reduces = if ctx.spot {
        side == Side::Sell
    } else {
        match o.position_side {
            Some(leg) => side == leg.closing_side(),
            None => o.close_position || o.reduce_only,
        }
    };
    Some(SymbolLine {
        symbol: o.symbol.clone(),
        line: OrderLine {
            price,
            kind: if reduces {
                LineKind::Target
            } else {
                LineKind::Entry
            },
            side,
            qty: o.qty,
            whole: false,
            stale,
            grab: Grab::No,
            contracts: ctx.contracts,
        },
    })
}

fn position_line(p: &HeldPosition, stale: bool, ctx: &Ctx<'_>) -> Option<SymbolLine> {
    let price = positive(p.entry_price)?;
    Some(SymbolLine {
        symbol: p.symbol.clone(),
        line: OrderLine {
            price,
            kind: LineKind::Position,
            side: match p.dir {
                Dir::Long => Side::Buy,
                Dir::Short => Side::Sell,
            },
            qty: Some(p.qty),
            whole: false,
            stale,
            grab: Grab::No,
            // An OKX position's size (`pos`) is in contracts as well.
            contracts: ctx.contracts,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use exchange::trade::stream::PositionSide;

    fn d(s: &str) -> Decimal {
        Decimal::parse(s).unwrap()
    }

    fn algo(role: ProtectRole, side: Side, trigger: Option<&str>) -> OpenOrder {
        OpenOrder {
            symbol: "BTCUSDT".into(),
            id: "1".into(),
            kind: OpenKind::Algo(None),
            side: Some(side),
            position_side: None,
            qty: Some(d("0.5")),
            trigger_price: trigger.map(d),
            close_position: false,
            reduce_only: true,
            role: Some(role),
            attached: None,
            client_id: None,
            limit: false,
            limit_price: None,
            activation_price: None,
            callback_rate: None,
            price: None,
        }
    }

    fn held(symbol: &str, dir: Dir, entry: Option<&str>) -> HeldPosition {
        HeldPosition {
            symbol: symbol.into(),
            side: PositionSide::Both,
            dir,
            qty: d("2"),
            margin: None,
            entry_price: entry.map(d),
        }
    }

    fn kinds(v: &[SymbolLine]) -> Vec<LineKind> {
        v.iter().map(|l| l.line.kind).collect()
    }

    #[test]
    fn a_one_way_stop_and_target_become_lines_with_their_size() {
        let v = lines_of(
            &[
                algo(ProtectRole::Stop, Side::Sell, Some("58000")),
                algo(ProtectRole::Target, Side::Sell, Some("65000")),
            ],
            &[held("BTCUSDT", Dir::Long, Some("60000"))],
            false,
        );
        assert_eq!(
            kinds(&v),
            [LineKind::Stop, LineKind::Target, LineKind::Position]
        );
        assert_eq!(v[0].line.price, d("58000"));
        assert_eq!(v[0].line.qty, Some(d("0.5")));
        assert_eq!(v[0].line.label(), "SL 0.5");
        assert_eq!(v[2].line.side, Side::Buy);
        assert_eq!(v[2].line.label(), "Pos 2");
    }

    #[test]
    fn a_hedge_leg_is_told_by_its_position_side() {
        let mut close_long = algo(ProtectRole::Stop, Side::Sell, Some("58000"));
        close_long.position_side = Some(Dir::Long);
        close_long.reduce_only = false;
        // The short leg's stop is a buy; a sell on the short leg opens more of it.
        let mut close_short = algo(ProtectRole::Stop, Side::Buy, Some("62000"));
        close_short.position_side = Some(Dir::Short);
        close_short.reduce_only = false;
        let mut add_short = algo(ProtectRole::Stop, Side::Sell, Some("57000"));
        add_short.position_side = Some(Dir::Short);
        add_short.reduce_only = false;
        let v = lines_of(&[close_long, close_short, add_short], &[], false);
        assert_eq!(kinds(&v), [LineKind::Stop, LineKind::Stop, LineKind::Entry]);
        assert_eq!(v[1].line.side, Side::Buy);
    }

    #[test]
    fn an_algo_with_only_a_trigger_is_drawn_without_a_size() {
        let mut o = algo(ProtectRole::Stop, Side::Sell, Some("58000"));
        o.qty = None;
        o.close_position = true;
        let v = lines_of(&[o], &[], false);
        assert_eq!(v[0].line.qty, None);
        assert_eq!(v[0].line.label(), "SL all");
        let mut bare = algo(ProtectRole::Stop, Side::Sell, Some("58000"));
        bare.qty = None;
        assert_eq!(lines_of(&[bare], &[], false)[0].line.label(), "SL");
    }

    #[test]
    fn an_okx_entry_with_attached_orders_is_not_drawn_as_protection() {
        let mut o = algo(ProtectRole::Stop, Side::Buy, Some("58000"));
        o.kind = OpenKind::Plain;
        o.role = None;
        o.attached = Some(ProtectRole::StopAndTarget);
        assert!(lines_of(&[o], &[], false).is_empty());
    }

    #[test]
    fn a_row_missing_a_field_is_not_drawn() {
        let mut no_side = algo(ProtectRole::Stop, Side::Sell, Some("58000"));
        no_side.side = None;
        let no_price = algo(ProtectRole::Stop, Side::Sell, None);
        let zero = algo(ProtectRole::Stop, Side::Sell, Some("0"));
        let mut plain = algo(ProtectRole::Stop, Side::Sell, Some("58000"));
        plain.role = None;
        assert!(lines_of(&[no_side, no_price, zero, plain], &[], false).is_empty());
        // A position with no entry price (a snapshot reads none) has no line either.
        assert!(lines_of(&[], &[held("BTCUSDT", Dir::Long, None)], false).is_empty());
    }

    fn limit(side: Side, price: Option<&str>) -> OpenOrder {
        let mut o = algo(ProtectRole::Stop, side, None);
        o.kind = OpenKind::Plain;
        o.role = None;
        o.reduce_only = false;
        o.price = price.map(d);
        o
    }

    #[test]
    fn a_plain_limit_that_adds_is_an_entry_at_its_price() {
        let v = lines_of(&[limit(Side::Buy, Some("59000"))], &[], false);
        assert_eq!(kinds(&v), [LineKind::Entry]);
        assert_eq!(v[0].line.price, d("59000"));
        assert_eq!(v[0].line.side, Side::Buy);
        assert_eq!(v[0].line.label(), "Entry 0.5");
    }

    #[test]
    fn a_plain_limit_that_reduces_is_a_target() {
        let mut one_way = limit(Side::Sell, Some("65000"));
        one_way.reduce_only = true;
        let mut hedge_close = limit(Side::Sell, Some("66000"));
        hedge_close.position_side = Some(Dir::Long);
        let mut hedge_add = limit(Side::Sell, Some("67000"));
        hedge_add.position_side = Some(Dir::Short);
        let v = lines_of(&[one_way, hedge_close, hedge_add], &[], false);
        assert_eq!(
            kinds(&v),
            [LineKind::Target, LineKind::Target, LineKind::Entry]
        );
    }

    #[test]
    fn a_plain_row_without_a_usable_price_or_side_is_not_drawn() {
        let mut no_side = limit(Side::Buy, Some("59000"));
        no_side.side = None;
        let zero = limit(Side::Buy, Some("0"));
        let none = limit(Side::Buy, None);
        let mut list = limit(Side::Buy, Some("59000"));
        list.kind = OpenKind::List;
        assert!(lines_of(&[no_side, zero, none, list], &[], false).is_empty());
    }

    #[test]
    fn a_trailing_stop_needs_a_price_and_says_trailing() {
        let none = algo(ProtectRole::Trailing, Side::Sell, None);
        assert!(lines_of(&[none], &[], false).is_empty());
        let mut with_activation = algo(ProtectRole::Trailing, Side::Sell, None);
        with_activation.activation_price = Some(d("61000"));
        let v = lines_of(&[with_activation], &[], false);
        assert_eq!(v[0].line.kind, LineKind::Trailing);
        assert_eq!(v[0].line.label(), "Trail 0.5 trailing");
        // An activation price is for a trailing order only.
        let mut stop = algo(ProtectRole::Stop, Side::Sell, None);
        stop.activation_price = Some(d("61000"));
        assert!(lines_of(&[stop], &[], false).is_empty());
    }

    #[test]
    fn stale_lists_make_every_line_stale() {
        let v = lines_of(
            &[algo(ProtectRole::Stop, Side::Sell, Some("58000"))],
            &[held("BTCUSDT", Dir::Short, Some("60000"))],
            true,
        );
        assert!(v.iter().all(|l| l.line.stale));
        assert_eq!(v[0].line.label(), "SL 0.5 · stale");
        assert_eq!(v[1].line.side, Side::Sell);
    }

    /// Condition 78: a line is never saved. The types cannot be serialized, nothing the saved
    /// state is made of names them (`data` cannot, it does not depend on this crate), and the
    /// whole saved state of a run that holds lines has neither a line nor an order id.
    #[test]
    fn order_lines_never_reach_the_saved_state() {
        use std::marker::PhantomData;
        trait Fallback {
            const SERIALIZABLE: bool = false;
        }
        impl<T> Fallback for T {}
        struct Probe<T>(PhantomData<T>);
        impl<T: serde::Serialize> Probe<T> {
            const SERIALIZABLE: bool = true;
        }
        const { assert!(!Probe::<OrderLine>::SERIALIZABLE) };
        const { assert!(!Probe::<SymbolLine>::SERIALIZABLE) };
        const { assert!(!Probe::<LineKind>::SERIALIZABLE) };
        // The probe does tell a serializable type apart.
        const { assert!(Probe::<String>::SERIALIZABLE) };

        let mut o = algo(ProtectRole::Stop, Side::Sell, Some("58123.5"));
        o.id = "ORDER-ID-9137".into();
        let lines = lines_of(&[o], &[held("BTCUSDT", Dir::Long, Some("59876.5"))], true);
        assert_eq!(lines.len(), 2);
        let state = serde_json::to_string(&data::State::default()).unwrap();
        for needle in ["ORDER-ID-9137", "58123.5", "59876.5", "stale", "LineKind"] {
            assert!(!state.contains(needle), "the saved state names {needle}");
        }

        // The data crate (state, layouts, drawings library) does not refer to the view model.
        fn scan(dir: &std::path::Path, hits: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    scan(&path, hits);
                } else if path.extension().is_some_and(|e| e == "rs")
                    && let Ok(text) = std::fs::read_to_string(&path)
                    && (text.contains("OrderLine") || text.contains("order_lines"))
                {
                    hits.push(path.display().to_string());
                }
            }
        }
        let mut hits = Vec::new();
        scan(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("data/src"),
            &mut hits,
        );
        assert!(hits.is_empty(), "data refers to order lines: {hits:?}");
    }

    #[test]
    fn the_price_in_book_units_is_exact() {
        let l = lines_of(
            &[algo(ProtectRole::Stop, Side::Sell, Some("58000.25"))],
            &[],
            false,
        );
        assert_eq!(l[0].line.price_units(), Some(5_800_025_000_000_000));
        let small = lines_of(
            &[algo(ProtectRole::Stop, Side::Sell, Some("0.000123"))],
            &[],
            false,
        );
        assert_eq!(small[0].line.price_units(), Some(12_300_000));
    }

    #[test]
    fn the_price_for_an_axis_is_a_finite_positive_float() {
        let l = lines_of(
            &[algo(ProtectRole::Stop, Side::Sell, Some("58000.5"))],
            &[],
            false,
        );
        assert_eq!(l[0].line.price_f32(), Some(58000.5));
    }

    fn with_ctx(orders: &[OpenOrder], spot: bool, contracts: bool, yes: bool) -> Vec<SymbolLine> {
        let grab = move |o: &OpenOrder| {
            if yes {
                Grab::Yes(o.id.clone())
            } else {
                Grab::No
            }
        };
        lines_with(
            orders,
            &[held("BTCUSDT", Dir::Long, Some("60000"))],
            false,
            &Ctx {
                spot,
                contracts,
                grab: &grab,
            },
        )
    }

    #[test]
    fn only_stop_and_target_rows_ask_for_a_grab() {
        let v = with_ctx(
            &[
                algo(ProtectRole::Stop, Side::Sell, Some("58000")),
                algo(ProtectRole::Target, Side::Sell, Some("65000")),
                limit(Side::Buy, Some("59000")),
            ],
            false,
            false,
            true,
        );
        let by_kind = |k: LineKind| v.iter().find(|l| l.line.kind == k).unwrap();
        assert_eq!(by_kind(LineKind::Stop).line.grab, Grab::Yes("1".into()));
        assert_eq!(by_kind(LineKind::Target).line.grab, Grab::Yes("1".into()));
        assert_eq!(by_kind(LineKind::Entry).line.grab, Grab::No);
        assert_eq!(by_kind(LineKind::Position).line.grab, Grab::No);
    }

    #[test]
    fn a_spot_sell_limit_is_a_target_and_a_spot_buy_limit_an_entry() {
        let sell = with_ctx(&[limit(Side::Sell, Some("70000"))], true, false, false);
        assert!(sell.iter().any(|l| l.line.kind == LineKind::Target));
        let buy = with_ctx(&[limit(Side::Buy, Some("50000"))], true, false, false);
        assert!(buy.iter().any(|l| l.line.kind == LineKind::Entry));
    }

    #[test]
    fn contract_sizes_are_labelled_with_their_unit() {
        let v = with_ctx(
            &[algo(ProtectRole::Stop, Side::Sell, Some("58000"))],
            false,
            true,
            false,
        );
        assert_eq!(v[0].line.label(), "SL 0.5 ct");
        let v = with_ctx(
            &[algo(ProtectRole::Stop, Side::Sell, Some("58000"))],
            false,
            false,
            false,
        );
        assert_eq!(v[0].line.label(), "SL 0.5");
    }
}
