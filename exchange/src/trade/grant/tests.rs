//! The grant against what the automatic path may send: one table per refusal.

use super::*;
use crate::account::PositionMode;
use crate::trade::binance_futures as bf;
use crate::trade::rules::fixtures::*;
use crate::trade::types::{
    Action, Exec, Protective, ProtectiveKind, Size, TimeInForce, WorkingType,
};

const NOW: i64 = 2_000;
const AGE: i64 = 60_000;
const VALID: i64 = 10_000;

fn key(n: u8) -> AccountKey {
    AccountKey::from_bytes([n; 16])
}

fn dup(c: &ClientId) -> ClientId {
    ClientId::parse(c.as_str()).unwrap()
}

struct Setup {
    table: crate::trade::rules::RulesTable,
    /// The client id of every entry this setup makes (one grant is made per test).
    entry_id: ClientId,
}

fn spec() -> ProtectSpec {
    use crate::trade::protect::{Offset, TargetSpec, TriggerBasis};
    ProtectSpec {
        stop: Some(Offset::Percent(d("1"))),
        targets: vec![
            TargetSpec {
                offset: Offset::Percent(d("2")),
                share_pct: 50,
            },
            TargetSpec {
                offset: Offset::Percent(d("4")),
                share_pct: 50,
            },
        ],
        trailing: None,
        trigger_price: TriggerBasis::Mark,
    }
}

/// A fill of the grant's own entry.
fn fill(g: &mut AutoGrant, qty: Decimal) {
    let id = g.entry_id.clone();
    assert!(g.add_fill(&id, qty));
}

impl Setup {
    fn new() -> Self {
        Setup {
            table: futures(),
            entry_id: ClientId::generate(Role::Entry),
        }
    }

    fn basis(&self) -> Basis {
        Basis::new(self.entry_id.as_str(), &spec())
    }

    fn fresh(&self) -> FreshRules<'_> {
        self.table.fresh_lookup("BTCUSDT", NOW, AGE).unwrap()
    }

    fn entry(&self, account: AccountKey, dir: Dir) -> OrderIntent {
        let rules = self.table.lookup("BTCUSDT").unwrap();
        let request = bf::new_order(bf::FuturesEntry {
            rules,
            mode: Some(PositionMode::OneWay),
            action: Action::Open(dir),
            qty: d("0.002"),
            exec: Exec::Limit {
                price: d("60000"),
                tif: TimeInForce::Gtc,
            },
            ref_price: Some(d("60000")),
            client_id: dup(&self.entry_id),
        })
        .unwrap();
        OrderIntent::binance(account, request, &self.fresh()).unwrap()
    }

    fn grant(&self, roles: GrantRoles) -> AutoGrant {
        AutoGrant::from_entry(&self.entry(key(1), Dir::Long), roles, &spec(), VALID, NOW).unwrap()
    }

    /// A protective order for `dir`; `kind` picks the type and the side of the trigger.
    fn protective(
        &self,
        account: AccountKey,
        mode: PositionMode,
        dir: Dir,
        kind: ProtectiveKind,
        size: Size,
        id: &ClientId,
    ) -> OrderIntent {
        let rules = self.table.lookup("BTCUSDT").unwrap();
        let below = (dir == Dir::Long) == kind.is_stop_side();
        let trigger = if below { "59000" } else { "61000" };
        let request = bf::new_algo_order(
            rules,
            Some(mode),
            Protective {
                kind,
                dir,
                size,
                trigger_price: Some(d(trigger)),
                limit_price: None,
                tif: TimeInForce::Gtc,
                working_type: WorkingType::MarkPrice,
                trailing: None,
                ref_price: d("60000"),
                client_id: dup(id),
            },
        )
        .unwrap();
        OrderIntent::binance(account, request, &self.fresh()).unwrap()
    }

    fn attach(
        &self,
        kind: ProtectiveKind,
        size: Size,
        id: &ClientId,
    ) -> Result<AutoIntent, TradeError> {
        AutoIntent::new(
            AutoPath::AttachProtective,
            self.protective(key(1), PositionMode::OneWay, Dir::Long, kind, size, id),
        )
        .map(|a| a.with_basis(self.basis()))
    }
}

const ALL: GrantRoles = GrantRoles {
    stop: true,
    targets: 2,
    trailing: false,
};

fn qty(s: &str) -> Size {
    Size::Qty(d(s))
}

#[test]
fn a_matching_request_is_authorised_once_and_only_once() {
    let s = Setup::new();
    let mut g = s.grant(ALL);
    fill(&mut g, d("0.001"));
    let (_, stop) = g.issue(Role::StopLoss, NOW).unwrap();
    let a = s
        .attach(ProtectiveKind::StopMarket, Size::ClosePosition, &stop)
        .unwrap();
    let granted = g.authorise(a, NOW).unwrap();
    assert_eq!(granted.path(), AutoPath::AttachProtective);
    // The same id again, as a new request: refused, nothing is sent twice.
    let again = s
        .attach(ProtectiveKind::StopMarket, Size::ClosePosition, &stop)
        .unwrap();
    assert_eq!(
        g.authorise(again, NOW).unwrap_err(),
        GrantRefusal::IdNotAvailable
    );
}

#[test]
fn a_request_for_another_account_or_symbol_is_refused() {
    let s = Setup::new();
    let mut g = s.grant(ALL);
    fill(&mut g, d("0.002"));
    let (_, id) = g.issue(Role::StopLoss, NOW).unwrap();
    let other_account = AutoIntent::new(
        AutoPath::AttachProtective,
        s.protective(
            key(2),
            PositionMode::OneWay,
            Dir::Long,
            ProtectiveKind::StopMarket,
            Size::ClosePosition,
            &id,
        ),
    )
    .unwrap();
    assert_eq!(
        g.authorise(other_account, NOW).unwrap_err(),
        GrantRefusal::WrongAccount
    );
    // Not spent by the refusal: the right request still goes through.
    g.symbol = "ETHUSDT".to_string();
    let a = s
        .attach(ProtectiveKind::StopMarket, Size::ClosePosition, &id)
        .unwrap();
    assert_eq!(g.authorise(a, NOW).unwrap_err(), GrantRefusal::WrongSymbol);
    g.symbol = "BTCUSDT".to_string();
    let a = s
        .attach(ProtectiveKind::StopMarket, Size::ClosePosition, &id)
        .unwrap();
    assert!(g.authorise(a, NOW).is_ok());
}

#[test]
fn an_order_that_adds_exposure_never_becomes_an_auto_intent() {
    let s = Setup::new();
    let entry = s.entry(key(1), Dir::Long);
    assert_eq!(
        AutoIntent::new(AutoPath::AttachProtective, entry).unwrap_err(),
        TradeError::AutoPathMustReduce
    );
}

#[test]
fn the_quantity_cannot_exceed_what_is_filled() {
    let s = Setup::new();
    let mut g = s.grant(ALL);
    let (_, tp) = g.issue(Role::TakeProfit, NOW).unwrap();
    let a = s
        .attach(ProtectiveKind::TakeProfitMarket, qty("0.001"), &tp)
        .unwrap();
    // Nothing filled yet.
    assert_eq!(g.authorise(a, NOW).unwrap_err(), GrantRefusal::NoFill);
    fill(&mut g, d("0.001"));
    let big = s
        .attach(ProtectiveKind::TakeProfitMarket, qty("0.002"), &tp)
        .unwrap();
    assert_eq!(
        g.authorise(big, NOW).unwrap_err(),
        GrantRefusal::QtyTooLarge
    );
    let exact = s
        .attach(ProtectiveKind::TakeProfitMarket, qty("0.001"), &tp)
        .unwrap();
    assert!(g.authorise(exact, NOW).is_ok());
}

#[test]
fn fills_are_capped_at_the_entry_quantity() {
    let s = Setup::new();
    let mut g = s.grant(ALL);
    fill(&mut g, d("0.0015"));
    fill(&mut g, d("0.0015"));
    assert_eq!(g.filled(), d("0.002"));
    fill(&mut g, d("-1"));
    assert_eq!(g.filled(), d("0.002"));
}

#[test]
fn a_role_outside_the_list_or_issued_twice_is_refused() {
    let s = Setup::new();
    let mut g = s.grant(GrantRoles {
        stop: true,
        targets: 1,
        trailing: false,
    });
    assert_eq!(
        g.issue(Role::Trailing, NOW).unwrap_err(),
        GrantRefusal::RoleNotListed
    );
    assert_eq!(
        g.issue(Role::Entry, NOW).unwrap_err(),
        GrantRefusal::RoleNotListed
    );
    assert!(g.issue(Role::StopLoss, NOW).is_ok());
    assert_eq!(
        g.issue(Role::StopLoss, NOW).unwrap_err(),
        GrantRefusal::RoleSpent
    );
    assert!(g.issue(Role::TakeProfit, NOW).is_ok());
    assert_eq!(
        g.issue(Role::TakeProfit, NOW).unwrap_err(),
        GrantRefusal::RoleSpent
    );
}

#[test]
fn a_request_of_another_role_than_its_id_is_refused() {
    let s = Setup::new();
    let mut g = s.grant(ALL);
    fill(&mut g, d("0.002"));
    let (_, stop_id) = g.issue(Role::StopLoss, NOW).unwrap();
    // A take-profit sent under the stop's id.
    let a = s
        .attach(ProtectiveKind::TakeProfitMarket, qty("0.001"), &stop_id)
        .unwrap();
    assert_eq!(
        g.authorise(a, NOW).unwrap_err(),
        GrantRefusal::RoleNotListed
    );
    // An id the grant never issued.
    let stranger = ClientId::generate(Role::StopLoss);
    let a = s
        .attach(ProtectiveKind::StopMarket, Size::ClosePosition, &stranger)
        .unwrap();
    assert_eq!(g.authorise(a, NOW).unwrap_err(), GrantRefusal::UnknownId);
}

#[test]
fn a_late_request_is_refused_but_a_cancel_is_not() {
    let s = Setup::new();
    let mut g = s.grant(ALL);
    fill(&mut g, d("0.002"));
    let (stop_ref, stop) = g.issue(Role::StopLoss, NOW).unwrap();
    let (_, tp) = g.issue(Role::TakeProfit, NOW).unwrap();
    let late = NOW + VALID + 1;
    assert_eq!(
        g.issue(Role::TakeProfit, late).unwrap_err(),
        GrantRefusal::Expired
    );
    let a = s
        .attach(ProtectiveKind::StopMarket, Size::ClosePosition, &stop)
        .unwrap();
    assert!(g.authorise(a, NOW).is_ok());
    let a = s
        .attach(ProtectiveKind::TakeProfitMarket, qty("0.001"), &tp)
        .unwrap();
    assert_eq!(g.authorise(a, late).unwrap_err(), GrantRefusal::Expired);
    // The sibling cancel still works long after the attach window.
    let cancel = g.cancel_intent(stop_ref, &s.fresh()).unwrap();
    assert!(g.authorise(cancel, late).is_ok());
}

#[test]
fn the_side_and_position_side_must_match_the_grant() {
    let s = Setup::new();
    let mut g = s.grant(ALL);
    fill(&mut g, d("0.002"));
    let (_, id) = g.issue(Role::StopLoss, NOW).unwrap();
    // A stop for a short position closes with BUY: not the closing side of a long.
    let short = AutoIntent::new(
        AutoPath::AttachProtective,
        s.protective(
            key(1),
            PositionMode::OneWay,
            Dir::Short,
            ProtectiveKind::StopMarket,
            Size::ClosePosition,
            &id,
        ),
    )
    .unwrap();
    assert_eq!(
        g.authorise(short, NOW).unwrap_err(),
        GrantRefusal::WrongSide
    );
    // Hedge order for the other leg, with the closing side of a long but SHORT's position side.
    let mut g2 = s.grant(ALL);
    fill(&mut g2, d("0.002"));
    let (_, id2) = g2.issue(Role::StopLoss, NOW).unwrap();
    let hedge_long = AutoIntent::new(
        AutoPath::AttachProtective,
        s.protective(
            key(1),
            PositionMode::Hedge,
            Dir::Long,
            ProtectiveKind::StopMarket,
            Size::ClosePosition,
            &id2,
        ),
    )
    .unwrap()
    .with_basis(s.basis());
    assert!(g2.authorise(hedge_long, NOW).is_ok());
}

#[test]
fn a_cancel_covers_only_an_order_the_grant_sent_and_only_once() {
    let s = Setup::new();
    let mut g = s.grant(ALL);
    fill(&mut g, d("0.002"));
    let (stop_ref, stop) = g.issue(Role::StopLoss, NOW).unwrap();
    // Not sent yet: there is nothing to cancel.
    assert_eq!(
        g.cancel_intent(stop_ref, &s.fresh()).unwrap_err(),
        GrantRefusal::IdNotAvailable
    );
    let a = s
        .attach(ProtectiveKind::StopMarket, Size::ClosePosition, &stop)
        .unwrap();
    g.authorise(a, NOW).unwrap();
    let cancel = g.cancel_intent(stop_ref, &s.fresh()).unwrap();
    let granted = g.authorise(cancel, NOW).unwrap();
    assert_eq!(granted.path(), AutoPath::CancelSibling);
    // A repeated event never cancels twice.
    assert_eq!(
        g.cancel_intent(stop_ref, &s.fresh()).unwrap_err(),
        GrantRefusal::AlreadyCancelled
    );
}

#[test]
fn a_cancel_of_a_foreign_order_or_under_the_wrong_path_is_refused() {
    let s = Setup::new();
    let mut g = s.grant(ALL);
    fill(&mut g, d("0.002"));
    let (_, stop) = g.issue(Role::StopLoss, NOW).unwrap();
    let stranger = ClientId::generate(Role::StopLoss);
    let foreign = OrderIntent::binance(
        key(1),
        bf::cancel_algo_order(&OrderRef::Client(stranger)).unwrap(),
        &s.fresh(),
    )
    .unwrap();
    let foreign = AutoIntent::new(AutoPath::CancelSibling, foreign).unwrap();
    assert_eq!(
        g.authorise(foreign, NOW).unwrap_err(),
        GrantRefusal::UnknownId
    );
    // A cancel of an issued id sent as an attach, and a placement sent as a cancel.
    let cancel = OrderIntent::binance(
        key(1),
        bf::cancel_algo_order(&OrderRef::Client(dup(&stop))).unwrap(),
        &s.fresh(),
    )
    .unwrap();
    let wrong = AutoIntent::new(AutoPath::AttachProtective, cancel).unwrap();
    assert_eq!(
        g.authorise(wrong, NOW).unwrap_err(),
        GrantRefusal::PathMismatch
    );
    let place = s
        .attach(ProtectiveKind::StopMarket, Size::ClosePosition, &stop)
        .unwrap()
        .intent;
    let wrong = AutoIntent::new(AutoPath::CancelSibling, place).unwrap();
    assert_eq!(
        g.authorise(wrong, NOW).unwrap_err(),
        GrantRefusal::PathMismatch
    );
}

#[test]
fn a_grant_is_made_only_from_a_binance_entry_that_adds() {
    let s = Setup::new();
    let entry = s.entry(key(1), Dir::Short);
    let g = AutoGrant::from_entry(&entry, ALL, &spec(), VALID, NOW).unwrap();
    assert_eq!(g.dir(), Dir::Short);
    assert_eq!(g.filled(), Decimal::ZERO);
    assert_eq!(
        AutoGrant::from_entry(&entry, GrantRoles::default(), &spec(), VALID, NOW).unwrap_err(),
        GrantRefusal::Unsupported
    );
    let too_many = GrantRoles { targets: 4, ..ALL };
    assert_eq!(
        AutoGrant::from_entry(&entry, too_many, &spec(), VALID, NOW).unwrap_err(),
        GrantRefusal::Unsupported
    );
    // A request that is not an entry (a protective order) has no grant.
    let (_, id) = s.grant(ALL).issue(Role::StopLoss, NOW).unwrap();
    let not_entry = s.protective(
        key(1),
        PositionMode::OneWay,
        Dir::Long,
        ProtectiveKind::StopMarket,
        Size::ClosePosition,
        &id,
    );
    assert_eq!(
        AutoGrant::from_entry(&not_entry, ALL, &spec(), VALID, NOW).unwrap_err(),
        GrantRefusal::Unsupported
    );
}

#[test]
fn the_grant_prints_nothing() {
    let s = Setup::new();
    let g = s.grant(ALL);
    assert_eq!(format!("{g:?}"), "AutoGrant(<redacted>)");
}

#[test]
fn a_placement_must_name_the_grants_entry_and_the_confirmed_settings() {
    use crate::trade::protect::Offset;
    let s = Setup::new();
    let table: Vec<(&str, Option<Basis>, GrantRefusal)> = vec![
        ("no basis", None, GrantRefusal::NotBound),
        (
            "another entry",
            Some(Basis::new(
                ClientId::generate(Role::Entry).as_str(),
                &spec(),
            )),
            GrantRefusal::WrongEntry,
        ),
        (
            "another stop distance",
            Some(Basis::new(
                s.entry_id.as_str(),
                &ProtectSpec {
                    stop: Some(Offset::Percent(d("5"))),
                    ..spec()
                },
            )),
            GrantRefusal::SpecMismatch,
        ),
        (
            "another share",
            Some(Basis::new(s.entry_id.as_str(), &{
                let mut other = spec();
                other.targets[0].share_pct = 60;
                other
            })),
            GrantRefusal::SpecMismatch,
        ),
        (
            "another trigger basis",
            Some(Basis::new(
                s.entry_id.as_str(),
                &ProtectSpec {
                    trigger_price: crate::trade::protect::TriggerBasis::Last,
                    ..spec()
                },
            )),
            GrantRefusal::SpecMismatch,
        ),
    ];
    for (name, basis, want) in table {
        let mut g = s.grant(ALL);
        fill(&mut g, d("0.002"));
        let (_, id) = g.issue(Role::StopLoss, NOW).unwrap();
        let mut auto = s
            .attach(ProtectiveKind::StopMarket, Size::ClosePosition, &id)
            .unwrap();
        auto.basis = basis;
        let got = g.authorise(auto, NOW).err();
        assert_eq!(got, Some(want), "{name}");
    }
}

#[test]
fn a_fill_of_another_entry_is_not_added() {
    let s = Setup::new();
    let mut g = s.grant(ALL);
    let other = ClientId::generate(Role::Entry);
    assert!(!g.add_fill(other.as_str(), d("0.002")));
    assert!(!g.filled().is_positive());
    let own = s.entry_id.as_str().to_string();
    assert!(g.add_fill(&own, d("0.001")));
    assert_eq!(g.filled(), d("0.001"));
}

#[test]
fn the_take_profit_legs_together_stay_within_what_filled() {
    let s = Setup::new();
    let mut g = s.grant(GrantRoles {
        stop: false,
        targets: 3,
        trailing: false,
    });
    fill(&mut g, d("0.002"));
    let place = |g: &mut AutoGrant, q: &str| {
        let (_, id) = g.issue(Role::TakeProfit, NOW).unwrap();
        let a = s
            .attach(ProtectiveKind::TakeProfitMarket, qty(q), &id)
            .unwrap();
        g.authorise(a, NOW).map(|_| ())
    };
    place(&mut g, "0.001").unwrap();
    place(&mut g, "0.001").unwrap();
    // Each leg alone fits the fill; the three together would be more than what filled.
    assert_eq!(
        place(&mut g, "0.001").unwrap_err(),
        GrantRefusal::TotalTooLarge
    );
}
