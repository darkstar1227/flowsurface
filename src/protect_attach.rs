//! Protection that goes with an entry (M5 d2): which settings apply to one entry, what the
//! confirmation lists, and the in-memory table of what was listed once the entry was confirmed.
//!
//! Nothing here sends anything. OKX carries its protection on the entry itself (the builder
//! adds `attachAlgoOrds`, so it goes through the same confirmation and the same send as the
//! entry); on Binance futures the protection would follow the fill, and **nothing consumes the
//! table yet** (d3): the table only remembers what the confirmation showed.
//!
//! One rule decides what applies: the account's saved defaults, or the ticket's override for
//! this order, both checked by `ProtectDefaults::spec`. The account's auto-attach switch (and,
//! for a Live account, its second switch) is read from the saved defaults only: an override can
//! narrow or change the values, never switch attaching on.

use crate::order_flow::SendResult;
use data::config::exchange_accounts::AccountMeta;
use data::config::protect::ProtectDefaults;
use exchange::account::{AccountEnv, AccountProduct, PositionMode};
use exchange::trade::{
    Decimal, Dir, Offset, PreviewField, ProtectPlan, ProtectSpec, SymbolRules, TriggerBasis,
    entry_protection,
};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt;
use std::time::{Duration, Instant};
use uuid::Uuid;

/// What applies to one entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// No protection is set (or the override turned it off).
    Nothing,
    /// Protection is set but the account's switch is off: nothing is attached, and the
    /// confirmation says so. `live` is for the wording of a Live account.
    SwitchOff { live: bool },
    /// The checked settings.
    Attach(ProtectSpec),
}

/// The settings that apply to an entry on `meta`: the override when there is one, else the
/// account's defaults. Checked by the same `ProtectDefaults::spec` either way. A contract
/// account whose position mode is unknown is refused when there is something to attach.
pub fn resolve(
    meta: &AccountMeta,
    mode: Option<PositionMode>,
    over: Option<&ProtectDefaults>,
) -> Result<Resolved, String> {
    let mut chosen = over.unwrap_or(&meta.protect).clone();
    // The trigger price type is the account's setting; the ticket does not change it.
    chosen.trigger_price = meta.protect.trigger_price;
    if chosen.is_empty() {
        return Ok(Resolved::Nothing);
    }
    if !meta.protect.attaches_automatically(meta.env) {
        return Ok(Resolved::SwitchOff {
            live: meta.env == AccountEnv::Live,
        });
    }
    let mode = match (meta.product, mode) {
        (AccountProduct::Spot, _) => PositionMode::OneWay,
        (AccountProduct::UsdM, Some(m)) => m,
        (AccountProduct::UsdM, None) => {
            return Err(
                "The position mode of the account is unknown. Test the account again.".to_string(),
            );
        }
    };
    chosen
        .spec(meta.venue, mode)
        .map(Resolved::Attach)
        .map_err(|e| format!("The protection settings cannot be used: {e}."))
}

/// What the account holds on the symbol of an entry, as far as the app knows.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Holdings {
    /// The position list may not match the exchange (no stream, not synchronised, stale).
    pub stale: bool,
    /// The direction of each position held on the symbol.
    pub dirs: Vec<Dir>,
}

const REDUCES: &str = "This order reduces or reverses a position; protective defaults apply to new \
     entries only. Choose \"No protection\" to send it.";
const HOLDINGS_UNKNOWN: &str = "The positions of the account cannot be told right now, so it is not \
     known whether this order reduces or reverses one; protective defaults apply to new entries \
     only. Choose \"No protection\" to send it.";

/// Protection is attached to new entries only. On a one-way contract account an order against a
/// held position reduces or reverses it, and an attached stop would be placed for a direction
/// the position may no longer have; the same when the position list cannot be trusted. Refused
/// (the user can pick "No protection"). A hedge account opens the position of the order's
/// direction, so it is not affected; a spot account has no position to reduce.
pub fn check_new_entry(
    product: AccountProduct,
    mode: Option<PositionMode>,
    dir: Dir,
    held: &Holdings,
) -> Result<(), String> {
    if product != AccountProduct::UsdM || mode != Some(PositionMode::OneWay) {
        return Ok(());
    }
    if held.stale {
        return Err(HOLDINGS_UNKNOWN.to_string());
    }
    if held.dirs.iter().any(|d| *d != dir) {
        return Err(REDUCES.to_string());
    }
    Ok(())
}

/// The settings checked against the price the entry would fill at (the limit price, or the
/// reference price of a market order): a stop distance that is not smaller than the price, or a
/// share too small for the step, is refused now, before anything is confirmed. Returns the
/// indicative prices.
pub fn check_against_price(
    rules: &SymbolRules,
    dir: Dir,
    base: &Decimal,
    qty: &Decimal,
    spec: &ProtectSpec,
) -> Result<ProtectPlan, String> {
    check_distance(spec, base)?;
    entry_protection(rules, dir, base, qty, spec).map_err(|_| {
        "The protective prices cannot be worked out from the entry price and the settings \
         (check the distances and the shares against the quantity step). Nothing was sent."
            .to_string()
    })
}

/// A stop-loss distance at or above the price, in either direction, is a typo or a wrong unit.
pub fn check_distance(spec: &ProtectSpec, base: &Decimal) -> Result<(), String> {
    match spec.stop {
        Some(Offset::Distance(d)) if d.checked_cmp(base) != Ok(Ordering::Less) => Err(
            "The stop-loss distance is not smaller than the price. Nothing was sent.".to_string(),
        ),
        _ => Ok(()),
    }
}

/// How the protection is carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachKind {
    /// OKX: part of the entry order itself; the exchange creates it after a full fill.
    OkxOnEntry,
    /// Binance futures: the app sends it after the entry fills (d3), only while it runs.
    AfterFill,
}

/// What the confirmation lists, kept with the confirmation so the table gets the same thing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachPlan {
    pub kind: AttachKind,
    pub dir: Dir,
    /// A resting limit entry (it can fill after the app is closed).
    pub limit: bool,
    pub spec: ProtectSpec,
    /// What was listed, one line each.
    pub lines: Vec<String>,
}

/// Per entry: what the confirmation says about protection. `None` in `Notes` for a request that
/// is not an entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachNote {
    /// No protection is set for this entry.
    Nothing,
    SwitchOff {
        live: bool,
    },
    Attach(AttachPlan),
}

/// `unit` is the quote currency of the price when it is known; a distance is in price units.
fn percent_or_distance(o: Offset, unit: Option<&str>) -> String {
    match o {
        Offset::Percent(p) => format!("{}% from the fill price", p.to_plain_string()),
        Offset::Distance(d) => format!(
            "{} {} from the fill price",
            d.to_plain_string(),
            unit.filter(|u| !u.is_empty()).unwrap_or("price units")
        ),
    }
}

fn basis_word(b: TriggerBasis) -> &'static str {
    match b {
        TriggerBasis::Last => "last",
        TriggerBasis::Mark => "mark",
    }
}

/// The quote currency of a symbol, when it can be read from the name (OKX `BTC-USDT-SWAP`, a
/// Binance name ending in a common quote). `None` otherwise: the text then says "price units".
pub fn quote_of(symbol: &str) -> Option<&str> {
    if let Some(q) = symbol.split('-').nth(1).filter(|q| !q.is_empty()) {
        return Some(q);
    }
    ["USDT", "USDC", "FDUSD", "BUSD"]
        .into_iter()
        .find(|q| symbol.len() > q.len() && symbol.ends_with(q))
        .map(|q| &symbol[symbol.len() - q.len()..])
}

/// One line per part of the protection, with the prices it works out to at `base`. `unit` is the
/// quote currency of a distance (`quote_of`), when known.
pub fn plan_lines(
    spec: &ProtectSpec,
    plan: &ProtectPlan,
    base: &Decimal,
    unit: Option<&str>,
) -> Vec<String> {
    let at = base.to_plain_string();
    let mut lines = Vec::new();
    if let (Some(o), Some(price)) = (spec.stop, plan.stop) {
        lines.push(format!(
            "Stop loss: {}, about {} at {at}",
            percent_or_distance(o, unit),
            price.to_plain_string()
        ));
    }
    let n = spec.targets.len();
    for (i, (t, leg)) in spec.targets.iter().zip(&plan.targets).enumerate() {
        let name = if n == 1 {
            "Take profit".to_string()
        } else {
            format!("Take profit {} of {n}", i + 1)
        };
        lines.push(format!(
            "{name}: {}, {}% of the quantity, about {} at {at}",
            percent_or_distance(t.offset, unit),
            t.share_pct,
            leg.trigger.to_plain_string()
        ));
    }
    if let Some(t) = &spec.trailing {
        let start = t
            .activation
            .map(|a| format!(", starting {}", percent_or_distance(a, unit)))
            .unwrap_or_default();
        lines.push(format!(
            "Trailing stop: callback {}%{start}",
            t.callback_pct.to_plain_string()
        ));
    }
    lines.push(format!(
        "Triggers watch the {} price.",
        basis_word(spec.trigger_price)
    ));
    lines
}

/// The attached TP and SL as the exchange will receive them: read from the preview's fields
/// (`attachAlgoOrds[0].*`), which are the builder's own output.
pub fn okx_attach_lines(fields: &[PreviewField]) -> Vec<String> {
    let f = |name: &str| {
        let full = format!("attachAlgoOrds[0].{name}");
        fields
            .iter()
            .find(|f| f.name == full)
            .map(|f| f.value.as_str())
    };
    let mut lines = Vec::new();
    for (label, trigger, ty) in [
        ("Take profit", "tpTriggerPx", "tpTriggerPxType"),
        ("Stop loss", "slTriggerPx", "slTriggerPxType"),
    ] {
        if let Some(px) = f(trigger) {
            let ty = f(ty).map(|t| format!(" ({t} price)")).unwrap_or_default();
            lines.push(format!("{label} trigger: {px}{ty}"));
        }
    }
    lines
}

/// Whether the preview carries an attached group (what OKX needs to see before it is listed).
pub fn preview_has_attachment(fields: &[PreviewField]) -> bool {
    fields
        .iter()
        .any(|f| f.name.starts_with("attachAlgoOrds[0]."))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub text: String,
    /// Shown as a warning.
    pub warn: bool,
}

fn line(text: &str) -> Line {
    Line {
        text: text.to_string(),
        warn: false,
    }
}

fn warn(text: &str) -> Line {
    Line {
        text: text.to_string(),
        warn: true,
    }
}

/// The protection block of the confirmation. One confirmation covers all of it.
pub fn confirmation_lines(note: &AttachNote, fields: &[PreviewField]) -> Vec<Line> {
    match note {
        AttachNote::Nothing => vec![warn(
            "No protective orders are set for this entry: the position will have no stop-loss.",
        )],
        AttachNote::SwitchOff { live } => vec![warn(if *live {
            "Auto-attach is not enabled for this Live account (it has its own switch). No \
             protective orders will be attached."
        } else {
            "Auto-attach is not enabled for this account. No protective orders will be attached."
        })],
        AttachNote::Attach(plan) => match plan.kind {
            AttachKind::OkxOnEntry => {
                let mut lines = vec![line("Attached to this entry:")];
                lines.extend(okx_attach_lines(fields).iter().map(|t| line(t)));
                lines.push(line(
                    "The exchange creates them only after the entry is fully filled; a partly \
                     filled entry is not protected yet. They stay in force if this app is closed.",
                ));
                lines
            }
            AttachKind::AfterFill => {
                // d3: when the after-fill send path exists, restore "Sent automatically after the entry
                // fills:" and drop the warning below (see docs/trading-security.md, d2 notes).
                let mut lines = vec![line(
                    "Planned after the entry fills (not sent by this build):",
                )];
                lines.extend(plan.lines.iter().map(|t| line(t)));
                lines.push(line(
                    "The stop-loss and the take-profit cancel each other: when one triggers, \
                     the app cancels the other.",
                ));
                lines.push(warn(
                    "This build does not send them yet: place the protective orders by hand \
                     after the fill.",
                ));
                lines.push(warn(
                    "This works only while the app is running. If it is closed, or the \
                     connection is lost, nothing is sent and the position stays unprotected. \
                     For protection that works offline use OKX, or place the protective orders \
                     by hand right after the fill.",
                ));
                lines
            }
        },
    }
}

/// What a send result means for an entry's remembered protection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retain {
    Keep,
    Remove,
}

/// Exhaustive on purpose: a new result must be decided here.
pub fn retention(result: &SendResult) -> Retain {
    match result {
        // The exchange has it, or may have it: the protection is still owed.
        SendResult::Accepted { .. }
        | SendResult::Found
        | SendResult::Unresolved
        | SendResult::Partial { .. } => Retain::Keep,
        // Nothing was placed, or it is presumed not placed.
        SendResult::Rejected { .. }
        | SendResult::NotSent(_)
        | SendResult::Refused(_)
        | SendResult::RulesExpired
        | SendResult::ConfirmationMismatch
        | SendResult::NotPlacedPresumed => Retain::Remove,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingAttach {
    pub account: Uuid,
    pub symbol: String,
    pub plan: AttachPlan,
}

/// The protection listed for each confirmed entry, keyed by the entry's client id. In memory
/// only, never saved, never logged: the ids are not printed (`Debug` shows a count).
#[derive(Default)]
pub struct PendingAttachTable {
    by_id: HashMap<String, PendingAttach>,
    /// The id of the entry being sent for an (account, symbol): the summary of a send does not
    /// carry the id.
    sending: HashMap<(Uuid, String), String>,
    refused: u32,
}

impl fmt::Debug for PendingAttachTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PendingAttachTable({} entries)", self.by_id.len())
    }
}

impl PendingAttachTable {
    /// The entry was confirmed and goes out now. Refused (`false`, and counted) when the
    /// (account, symbol) already has a send in flight: the older entry is never dropped quietly.
    pub fn register(&mut self, entry_id: String, pending: PendingAttach) -> bool {
        let key = (pending.account, pending.symbol.clone());
        if self.sending.contains_key(&key) {
            self.refused = self.refused.saturating_add(1);
            return false;
        }
        self.sending.insert(key, entry_id.clone());
        self.by_id.insert(entry_id, pending);
        true
    }

    /// How many registrations were refused because a send was already in flight.
    #[cfg(test)]
    pub fn refused_count(&self) -> u32 {
        self.refused
    }

    /// The send of the (account, symbol) ended.
    pub fn settle(&mut self, account: Uuid, symbol: &str, result: &SendResult) {
        let Some(id) = self.sending.remove(&(account, symbol.to_string())) else {
            return;
        };
        if retention(result) == Retain::Remove {
            self.by_id.remove(&id);
        }
    }

    /// The entry is known not to stand: forget it, and the send marker that names it.
    pub fn remove_entry(&mut self, client_id: &str) {
        self.by_id.remove(client_id);
        self.sending.retain(|_, id| id != client_id);
    }

    /// Symbols of the account with protection listed, sorted.
    pub fn pending_symbols(&self, account: Uuid) -> Vec<String> {
        let mut v: Vec<String> = self
            .by_id
            .values()
            .filter(|p| p.account == account)
            .map(|p| p.symbol.clone())
            .collect();
        v.sort();
        v.dedup();
        v
    }

    pub fn has_pending(&self, account: Uuid, symbol: &str) -> bool {
        self.by_id
            .values()
            .any(|p| p.account == account && p.symbol == symbol)
    }

    /// A resting limit entry whose protection the app still owes: it may fill after the app is
    /// closed (condition 81). An OKX attachment is on the exchange's side and holds with the
    /// app closed, so it is not counted.
    pub fn has_resting_limit(&self) -> bool {
        self.by_id
            .values()
            .any(|p| p.plan.limit && p.plan.kind == AttachKind::AfterFill)
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    pub fn remove_account(&mut self, account: Uuid) {
        self.by_id.retain(|_, p| p.account != account);
        self.sending.retain(|(a, _), _| *a != account);
    }
}

/// How long the first close request stays "warned" before a second one is a plain close.
pub const CLOSE_WARNING_WINDOW: Duration = Duration::from_secs(10);

/// Whether a request to close the app is held back for a warning: there is a resting limit
/// entry with protection owed, and the user has not just been told. A second request within
/// the window closes.
pub fn hold_close(resting_limit: bool, warned_at: Option<Instant>, now: Instant) -> bool {
    resting_limit
        && warned_at.is_none_or(|t| now.saturating_duration_since(t) > CLOSE_WARNING_WINDOW)
}

pub const CLOSE_WARNING: &str = "A limit entry with protection is still resting. If it fills while this app is closed, nothing will protect the position. Close again to quit anyway.";

#[cfg(test)]
mod tests {
    use super::*;
    use data::config::protect::{ProtectOffset, TakeProfitLeg, TrailingDefault, TriggerPriceType};
    use exchange::account::{AccountVenue, PositionMode};
    use exchange::trade::TrailingSpec;

    fn d(s: &str) -> Decimal {
        Decimal::parse(s).unwrap()
    }

    fn meta(venue: AccountVenue, env: AccountEnv, mode: PositionMode) -> AccountMeta {
        AccountMeta::new("acct".into(), venue, env, AccountProduct::UsdM, mode)
    }

    fn pct(s: &str) -> ProtectOffset {
        ProtectOffset::Percent(s.into())
    }

    fn with_defaults(mut m: AccountMeta, p: ProtectDefaults) -> AccountMeta {
        m.protect = p;
        m
    }

    fn defaults(stop: Option<&str>, on: bool) -> ProtectDefaults {
        ProtectDefaults {
            stop_loss: stop.map(pct),
            auto_attach: on,
            ..Default::default()
        }
    }

    #[test]
    fn the_account_defaults_apply_when_there_is_no_override() {
        let m = with_defaults(
            meta(AccountVenue::Okx, AccountEnv::Testnet, PositionMode::OneWay),
            defaults(Some("2"), true),
        );
        let r = resolve(&m, Some(PositionMode::OneWay), None).unwrap();
        match r {
            Resolved::Attach(spec) => assert!(spec.stop.is_some()),
            other => panic!("{other:?}"),
        }
        // Nothing set: nothing to attach, switch or not.
        let none = meta(AccountVenue::Okx, AccountEnv::Testnet, PositionMode::OneWay);
        assert_eq!(
            resolve(&none, Some(PositionMode::OneWay), None),
            Ok(Resolved::Nothing)
        );
    }

    #[test]
    fn an_override_changes_or_turns_off_but_never_switches_attaching_on() {
        let on = with_defaults(
            meta(AccountVenue::Okx, AccountEnv::Testnet, PositionMode::OneWay),
            defaults(Some("2"), true),
        );
        let off = ProtectDefaults::default();
        assert_eq!(
            resolve(&on, Some(PositionMode::OneWay), Some(&off)),
            Ok(Resolved::Nothing)
        );
        let other = defaults(Some("5"), false);
        match resolve(&on, Some(PositionMode::OneWay), Some(&other)).unwrap() {
            Resolved::Attach(spec) => {
                assert_eq!(spec.stop, Some(Offset::Percent(d("5"))));
            }
            r => panic!("{r:?}"),
        }
        // The switch is off in the saved defaults: the override cannot turn it on, even with
        // its own flag set.
        let sw_off = with_defaults(
            meta(AccountVenue::Okx, AccountEnv::Testnet, PositionMode::OneWay),
            defaults(Some("2"), false),
        );
        let wants_on = defaults(Some("5"), true);
        assert_eq!(
            resolve(&sw_off, Some(PositionMode::OneWay), Some(&wants_on)),
            Ok(Resolved::SwitchOff { live: false })
        );
    }

    #[test]
    fn a_live_account_needs_its_second_switch() {
        let mut p = defaults(Some("2"), true);
        let m = with_defaults(
            meta(
                AccountVenue::Binance,
                AccountEnv::Live,
                PositionMode::OneWay,
            ),
            p.clone(),
        );
        assert_eq!(
            resolve(&m, Some(PositionMode::OneWay), None),
            Ok(Resolved::SwitchOff { live: true })
        );
        p.auto_attach_live = true;
        let m = with_defaults(m, p);
        assert!(matches!(
            resolve(&m, Some(PositionMode::OneWay), None),
            Ok(Resolved::Attach(_))
        ));
    }

    #[test]
    fn the_override_goes_through_the_same_checks_as_the_defaults() {
        let m = with_defaults(
            meta(
                AccountVenue::Binance,
                AccountEnv::Testnet,
                PositionMode::Hedge,
            ),
            defaults(None, true),
        );
        let legs = |n: &[(&str, u8)]| ProtectDefaults {
            take_profits: n
                .iter()
                .map(|(p, s)| TakeProfitLeg {
                    offset: pct(p),
                    share_pct: *s,
                })
                .collect(),
            ..Default::default()
        };
        // Several take-profit levels on a hedge account: refused, with the reason.
        let err = resolve(
            &m,
            Some(PositionMode::Hedge),
            Some(&legs(&[("1", 50), ("2", 50)])),
        )
        .unwrap_err();
        assert!(err.contains("one-way"), "{err}");
        // The same through the saved defaults.
        let saved = with_defaults(
            meta(
                AccountVenue::Binance,
                AccountEnv::Testnet,
                PositionMode::Hedge,
            ),
            ProtectDefaults {
                auto_attach: true,
                ..legs(&[("1", 50), ("2", 50)])
            },
        );
        assert!(resolve(&saved, Some(PositionMode::Hedge), None).is_err());
        // The mode comes from the exchange, not the saved account: a one-way report passes.
        assert!(
            resolve(
                &m,
                Some(PositionMode::OneWay),
                Some(&legs(&[("1", 50), ("2", 50)]))
            )
            .is_ok()
        );
        // A bad value and a total other than 100 are refused too.
        assert!(
            resolve(
                &m,
                Some(PositionMode::Hedge),
                Some(&defaults(Some("0"), true))
            )
            .is_err()
        );
        assert!(resolve(&m, Some(PositionMode::OneWay), Some(&legs(&[("1", 60)]))).is_err());
        // An unknown position mode refuses a contract account that has something to attach.
        assert!(resolve(&m, None, Some(&defaults(Some("2"), true))).is_err());
        // ... but not when there is nothing to attach.
        assert_eq!(
            resolve(&m, None, Some(&ProtectDefaults::default())),
            Ok(Resolved::Nothing)
        );
    }

    #[test]
    fn a_trailing_only_override_is_something_to_attach() {
        let m = with_defaults(
            meta(
                AccountVenue::Binance,
                AccountEnv::Testnet,
                PositionMode::OneWay,
            ),
            ProtectDefaults {
                auto_attach: true,
                trigger_price: TriggerPriceType::Mark,
                ..Default::default()
            },
        );
        let trail = ProtectDefaults {
            trailing: Some(TrailingDefault {
                callback_pct: "1".into(),
                activation: None,
            }),
            ..Default::default()
        };
        match resolve(&m, Some(PositionMode::OneWay), Some(&trail)).unwrap() {
            Resolved::Attach(spec) => {
                assert!(spec.trailing.is_some() && !spec.is_empty());
                // The account's trigger price type is kept.
                assert_eq!(spec.trigger_price, TriggerBasis::Mark);
            }
            r => panic!("{r:?}"),
        }
    }

    #[test]
    fn a_stop_distance_not_smaller_than_the_price_is_refused() {
        let spec = |stop: Offset| ProtectSpec {
            stop: Some(stop),
            ..ProtectSpec::default()
        };
        let base = d("50000");
        for bad in ["50000", "60000"] {
            let err = check_distance(&spec(Offset::Distance(d(bad))), &base).unwrap_err();
            assert!(err.contains("not smaller than the price"), "{bad}: {err}");
        }
        assert!(check_distance(&spec(Offset::Distance(d("49999.9"))), &base).is_ok());
        // A percent is limited by the settings check, and nothing else is a distance.
        assert!(check_distance(&spec(Offset::Percent(d("2"))), &base).is_ok());
        assert!(check_distance(&ProtectSpec::default(), &base).is_ok());
    }

    fn attach_plan(kind: AttachKind, limit: bool) -> AttachPlan {
        AttachPlan {
            kind,
            dir: Dir::Long,
            limit,
            spec: ProtectSpec {
                stop: Some(Offset::Percent(d("2"))),
                trailing: Some(TrailingSpec {
                    callback_pct: d("1"),
                    activation: None,
                }),
                ..ProtectSpec::default()
            },
            lines: vec!["Stop loss: 2% from the fill price".into()],
        }
    }

    fn pending(account: u128, symbol: &str, limit: bool) -> PendingAttach {
        PendingAttach {
            account: Uuid::from_u128(account),
            symbol: symbol.into(),
            plan: attach_plan(AttachKind::AfterFill, limit),
        }
    }

    #[test]
    fn an_entry_known_not_to_stand_leaves_the_table_by_its_client_id() {
        let mut t = PendingAttachTable::default();
        t.register("eAAAA".into(), pending(1, "BTCUSDT", true));
        t.register("eBBBB".into(), pending(1, "ETHUSDT", true));
        assert_eq!(
            t.pending_symbols(Uuid::from_u128(1)),
            vec!["BTCUSDT".to_string(), "ETHUSDT".to_string()]
        );
        t.remove_entry("eAAAA");
        assert!(!t.has_pending(Uuid::from_u128(1), "BTCUSDT"));
        assert!(t.has_pending(Uuid::from_u128(1), "ETHUSDT"));
        // Another id changes nothing; the account filter holds.
        t.remove_entry("eZZZZ");
        assert_eq!(t.len(), 1);
        assert!(t.pending_symbols(Uuid::from_u128(2)).is_empty());
    }

    #[test]
    fn every_send_result_has_a_retention() {
        use SendResult as R;
        let keep = [
            R::Accepted { order_id: None },
            R::Found,
            R::Unresolved,
            R::Partial {
                accepted: 1,
                rejected: 1,
            },
        ];
        let remove = [
            R::Rejected {
                code: "1".into(),
                msg: String::new(),
            },
            R::NotSent("x".into()),
            R::Refused("x".into()),
            R::RulesExpired,
            R::ConfirmationMismatch,
            R::NotPlacedPresumed,
        ];
        for r in &keep {
            assert_eq!(retention(r), Retain::Keep, "{r:?}");
        }
        for r in &remove {
            assert_eq!(retention(r), Retain::Remove, "{r:?}");
        }
    }

    #[test]
    fn the_table_registers_on_confirm_and_clears_by_result() {
        use SendResult as R;
        let a = Uuid::from_u128(1);
        let cases: Vec<(R, bool)> = vec![
            (R::Accepted { order_id: None }, true),
            (
                R::Rejected {
                    code: "-2010".into(),
                    msg: String::new(),
                },
                false,
            ),
            (R::NotSent("offline".into()), false),
            (R::Refused("gate".into()), false),
            (R::RulesExpired, false),
            (R::ConfirmationMismatch, false),
            (R::Found, true),
            (R::NotPlacedPresumed, false),
            (R::Unresolved, true),
        ];
        for (result, kept) in cases {
            let mut t = PendingAttachTable::default();
            t.register("eAAAA".into(), pending(1, "BTCUSDT", false));
            assert!(t.has_pending(a, "BTCUSDT"), "{result:?}");
            t.settle(a, "BTCUSDT", &result);
            assert_eq!(t.has_pending(a, "BTCUSDT"), kept, "{result:?}");
            assert_eq!(t.len(), usize::from(kept));
        }
    }

    #[test]
    fn a_result_for_another_pair_changes_nothing() {
        let mut t = PendingAttachTable::default();
        t.register("eAAAA".into(), pending(1, "BTCUSDT", false));
        t.settle(Uuid::from_u128(2), "BTCUSDT", &SendResult::RulesExpired);
        t.settle(Uuid::from_u128(1), "ETHUSDT", &SendResult::RulesExpired);
        assert!(t.has_pending(Uuid::from_u128(1), "BTCUSDT"));
        // A second result for the same send finds nothing to settle.
        t.settle(Uuid::from_u128(1), "BTCUSDT", &SendResult::Found);
        t.settle(Uuid::from_u128(1), "BTCUSDT", &SendResult::RulesExpired);
        assert!(t.has_pending(Uuid::from_u128(1), "BTCUSDT"));
        t.remove_account(Uuid::from_u128(1));
        assert_eq!(t.len(), 0);
        assert!(!format!("{t:?}").contains("eAAAA"));
    }

    #[test]
    fn only_a_resting_limit_entry_holds_back_the_close() {
        let mut t = PendingAttachTable::default();
        assert!(!t.has_resting_limit());
        t.register("eAAAA".into(), pending(1, "BTCUSDT", false));
        assert!(!t.has_resting_limit(), "a market entry is not resting");
        // An OKX limit entry carries its protection on the exchange: closing the app is fine.
        let mut okx = pending(1, "SOLUSDT", true);
        okx.plan.kind = AttachKind::OkxOnEntry;
        t.register("eOKXX".into(), okx);
        assert!(!t.has_resting_limit());
        t.register("eBBBB".into(), pending(1, "ETHUSDT", true));
        assert!(t.has_resting_limit());
        t.settle(
            Uuid::from_u128(1),
            "ETHUSDT",
            &SendResult::NotSent("x".into()),
        );
        assert!(!t.has_resting_limit());
    }

    #[test]
    fn closing_is_held_once_then_allowed() {
        let t0 = Instant::now();
        assert!(!hold_close(false, None, t0));
        assert!(hold_close(true, None, t0));
        assert!(!hold_close(true, Some(t0), t0 + Duration::from_secs(1)));
        assert!(hold_close(
            true,
            Some(t0),
            t0 + CLOSE_WARNING_WINDOW + Duration::from_secs(1)
        ));
    }

    #[test]
    fn the_confirmation_text_names_what_each_case_means() {
        let fields = |pairs: &[(&str, &str)]| -> Vec<PreviewField> {
            pairs
                .iter()
                .map(|(n, v)| PreviewField {
                    name: (*n).to_string(),
                    value: (*v).to_string(),
                })
                .collect()
        };
        let empty = fields(&[]);
        let attached = fields(&[
            ("attachAlgoOrds[0].slTriggerPx", "49000"),
            ("attachAlgoOrds[0].slTriggerPxType", "mark"),
            ("attachAlgoOrds[0].tpTriggerPx", "52000"),
            ("attachAlgoOrds[0].tpTriggerPxType", "mark"),
            ("sz", "1"),
        ]);
        assert!(preview_has_attachment(&attached) && !preview_has_attachment(&empty));
        assert_eq!(
            okx_attach_lines(&attached),
            vec![
                "Take profit trigger: 52000 (mark price)".to_string(),
                "Stop loss trigger: 49000 (mark price)".to_string()
            ]
        );
        let text = |note: &AttachNote| {
            confirmation_lines(note, &attached)
                .into_iter()
                .map(|l| l.text)
                .collect::<Vec<_>>()
                .join("\n")
        };
        // Binance futures: the warning about the app running, the mutual cancel.
        let after = text(&AttachNote::Attach(attach_plan(
            AttachKind::AfterFill,
            false,
        )));
        assert!(after.contains("only while the app is running"), "{after}");
        assert!(after.contains("unprotected"), "{after}");
        assert!(after.contains("cancel each other"), "{after}");
        assert!(after.contains("Stop loss: 2%"), "{after}");
        // OKX: no such warning; it says it works offline and not before a full fill.
        let okx = text(&AttachNote::Attach(attach_plan(
            AttachKind::OkxOnEntry,
            false,
        )));
        assert!(okx.contains("Stop loss trigger: 49000"), "{okx}");
        assert!(!okx.contains("only while the app is running"), "{okx}");
        assert!(okx.contains("fully filled"), "{okx}");
        // Off, and nothing set: said, not skipped.
        assert!(text(&AttachNote::SwitchOff { live: false }).contains("not enabled"));
        assert!(text(&AttachNote::SwitchOff { live: true }).contains("Live account"));
        assert!(text(&AttachNote::Nothing).contains("No protective orders"));
        // A warning is marked as one.
        assert!(
            confirmation_lines(&AttachNote::Nothing, &empty)
                .iter()
                .all(|l| l.warn)
        );
    }

    #[test]
    fn the_after_fill_block_says_it_is_not_sent_by_this_build() {
        let lines = confirmation_lines(
            &AttachNote::Attach(attach_plan(AttachKind::AfterFill, false)),
            &[],
        );
        assert_eq!(
            lines[0].text,
            "Planned after the entry fills (not sent by this build):"
        );
        assert!(
            lines.iter().any(|l| l.warn
                && l.text.contains(
                    "This build does not send them yet: place the protective orders by hand \
                     after the fill."
                )),
            "{lines:?}"
        );
        assert!(!lines.iter().any(|l| l.text.contains("Sent automatically")));
    }

    #[test]
    fn a_distance_names_its_quote_currency_or_price_units() {
        let o = Offset::Distance(d("150"));
        assert_eq!(
            percent_or_distance(o, Some("USDT")),
            "150 USDT from the fill price"
        );
        assert_eq!(
            percent_or_distance(o, None),
            "150 price units from the fill price"
        );
        assert_eq!(
            percent_or_distance(Offset::Percent(d("2")), Some("USDT")),
            "2% from the fill price"
        );
        assert_eq!(quote_of("BTC-USDT-SWAP"), Some("USDT"));
        assert_eq!(quote_of("BTCUSDT"), Some("USDT"));
        assert_eq!(quote_of("BTCEUR"), None);
    }

    #[test]
    fn a_second_registration_for_a_send_in_flight_is_refused() {
        let mut t = PendingAttachTable::default();
        assert!(t.register("eAAAA".into(), pending(1, "BTCUSDT", true)));
        assert!(!t.register("eBBBB".into(), pending(1, "BTCUSDT", false)));
        assert_eq!(t.refused_count(), 1);
        // The first entry is still listed, the second never was.
        assert_eq!(t.len(), 1);
        assert!(t.has_resting_limit());
        // Another symbol, and the same one after the send ended, are fine.
        assert!(t.register("eCCCC".into(), pending(1, "ETHUSDT", false)));
        t.settle(Uuid::from_u128(1), "BTCUSDT", &SendResult::Found);
        assert!(t.register("eDDDD".into(), pending(1, "BTCUSDT", false)));
    }

    fn held(stale: bool, dirs: &[Dir]) -> Holdings {
        Holdings {
            stale,
            dirs: dirs.to_vec(),
        }
    }

    #[test]
    fn one_way_contract_orders_against_a_position_or_unknown_positions_are_refused() {
        let one = Some(PositionMode::OneWay);
        let fut = AccountProduct::UsdM;
        // A short order while long: reduces or reverses.
        let e = check_new_entry(fut, one, Dir::Short, &held(false, &[Dir::Long])).unwrap_err();
        assert!(e.contains("reduces or reverses a position"), "{e}");
        assert!(e.contains("\"No protection\""), "{e}");
        // Stale or unknown lists: refused, and said.
        let e = check_new_entry(fut, one, Dir::Long, &held(true, &[])).unwrap_err();
        assert!(e.contains("cannot be told"), "{e}");
        // New entries and adding to the same side pass.
        assert!(check_new_entry(fut, one, Dir::Long, &held(false, &[])).is_ok());
        assert!(check_new_entry(fut, one, Dir::Long, &held(false, &[Dir::Long])).is_ok());
    }

    #[test]
    fn hedge_and_spot_accounts_are_not_affected_by_the_position_check() {
        let fut = AccountProduct::UsdM;
        let hedge = Some(PositionMode::Hedge);
        assert!(check_new_entry(fut, hedge, Dir::Short, &held(false, &[Dir::Long])).is_ok());
        assert!(check_new_entry(fut, hedge, Dir::Short, &held(true, &[])).is_ok());
        assert!(
            check_new_entry(
                AccountProduct::Spot,
                None,
                Dir::Short,
                &held(true, &[Dir::Long])
            )
            .is_ok()
        );
    }

    #[test]
    fn choosing_no_protection_leaves_nothing_to_check() {
        // The override with nothing set resolves to Nothing, which is never checked against
        // the positions (prepare_steps checks only `Resolved::Attach`): the reducing order can
        // be sent.
        let m = with_defaults(
            meta(AccountVenue::Okx, AccountEnv::Testnet, PositionMode::OneWay),
            defaults(Some("2"), true),
        );
        let off = ProtectDefaults::default();
        let r = resolve(&m, Some(PositionMode::OneWay), Some(&off)).unwrap();
        assert_eq!(r, Resolved::Nothing);
        // Without the override the same account resolves to Attach, which the check refuses.
        let r = resolve(&m, Some(PositionMode::OneWay), None).unwrap();
        assert!(matches!(r, Resolved::Attach(_)));
        assert!(
            check_new_entry(
                m.product,
                Some(PositionMode::OneWay),
                Dir::Short,
                &held(false, &[Dir::Long])
            )
            .is_err()
        );
    }
}
