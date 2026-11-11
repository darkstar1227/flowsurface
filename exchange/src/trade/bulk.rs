//! Cancel-all (condition 68) and flatten (condition 70). Both go through the single gate for
//! every request, then read the account again: success is only reported when nothing is left.
//! A request that cannot be built or is refused is counted as failed and the rest still run;
//! the re-read at the end always happens, so the report shows what really remains.

use super::binance_futures as fut;
use super::client::{OpenKind, OpenOrder, Position, ReadError, TradeClient};
use super::client_id::{ClientId, Role};
use super::gate::{AutoIntent, ConfirmToken, OrderIntent, RiskHook, SendGate, Spent};
use super::journal::UnknownJournal;
use super::okx::{self, OkxAlgoType};
use super::outcome::Outcome;
use super::queries;
use super::request::{BinanceRequest, OkxRequest};
use super::risk::{AutoCloseGrant, RiskNotice, RiskNoticeKind};
use super::rules::{FreshRules, RulesTable};
use super::types::{Action, Exec, MarginMode, OrderRef, TradeError};
use super::unknown::PendingUnknown;
use crate::account::{AccountProduct, AccountVenue, PositionMode};
use std::collections::BTreeMap;

/// What a bulk operation did. `remaining` is read from the exchange after the sends; the
/// operation only succeeded when it is empty and `failed` is zero.
#[derive(Debug)]
pub struct BulkReport<T> {
    pub sent: usize,
    /// Requests that were not accepted, were refused by the gate, or could not be built.
    pub failed: usize,
    pub remaining: Vec<T>,
    /// The final re-read failed, so `remaining` is not known (it is empty only because
    /// nothing could be read). The report is still returned so `sent`, `failed` and `pending`
    /// are not lost; the UI should say "result unknown, refresh".
    pub reread_failed: bool,
    /// Sends with an `Unknown` outcome; resolve them with `resolve_unknown`.
    pub pending: Vec<PendingUnknown>,
    /// OKX algo types that could not be read before the sends: orders of these types may
    /// exist, were not cancelled and are not in `remaining` (the final read, when made, shows
    /// what is really left). Never empty means the result is not clean.
    pub unknown_algo_types: Vec<OkxAlgoType>,
    /// At least one send went out without its write-ahead record (the journal is broken). The
    /// sends are not undone and `is_clear` does not look at this: the UI shows a standing
    /// warning instead.
    pub journal_failed: bool,
}

impl<T> BulkReport<T> {
    fn new() -> Self {
        Self {
            sent: 0,
            failed: 0,
            remaining: Vec::new(),
            reread_failed: false,
            pending: Vec::new(),
            unknown_algo_types: Vec::new(),
            journal_failed: false,
        }
    }

    /// Nothing is left, nothing failed, every order type could be read, and the final read
    /// worked.
    pub fn is_clear(&self) -> bool {
        !self.reread_failed
            && self.remaining.is_empty()
            && self.failed == 0
            && self.unknown_algo_types.is_empty()
    }
}

#[derive(Debug)]
pub enum BulkError {
    /// Reading the account failed before any send. A failed final re-read is reported in
    /// `BulkReport::reread_failed` instead.
    Read(ReadError),
    Trade(TradeError),
}

impl From<ReadError> for BulkError {
    fn from(e: ReadError) -> Self {
        BulkError::Read(e)
    }
}

impl From<TradeError> for BulkError {
    fn from(e: TradeError) -> Self {
        BulkError::Trade(e)
    }
}

/// How a bulk request is approved: by the user's confirmation, or on the automatic risk-close
/// path, where every request must reduce, close or cancel (condition 71).
enum Route {
    User(Spent),
    RiskClose,
}

/// A request to send for one symbol.
enum Built {
    Binance(BinanceRequest),
    Okx(Box<OkxRequest>),
}

impl TradeClient {
    async fn send_built<G: SendGate>(
        &self,
        gate: &G,
        risk: &dyn RiskHook,
        journal: &dyn UnknownJournal,
        route: &Route,
        built: Result<Built, TradeError>,
        fresh: &FreshRules<'_>,
        report: &mut BulkReport<impl Sized>,
    ) {
        let intent = built.and_then(|b| match b {
            Built::Binance(r) => OrderIntent::binance(self.account, r, fresh),
            Built::Okx(r) => OrderIntent::okx(self.account, *r, fresh),
        });
        let Ok(intent) = intent else {
            report.failed += 1;
            return;
        };
        let sent = match route {
            Route::User(spent) => {
                self.submit_spent(gate, risk, journal, spent.clone(), intent)
                    .await
            }
            Route::RiskClose => match AutoIntent::risk_close(intent) {
                Ok(auto) => self.submit_auto(gate, risk, journal, auto).await,
                // A request that would add exposure is never sent on this path.
                Err(_) => {
                    report.failed += 1;
                    return;
                }
            },
        };
        match sent {
            Ok(sub) => {
                report.sent += 1;
                if !matches!(sub.outcome, Outcome::Accepted { .. }) {
                    report.failed += 1;
                }
                report.pending.extend(sub.pending);
                report.journal_failed |= sub.journal_failed;
            }
            Err(_) => report.failed += 1,
        }
    }

    /// Cancel every open order of every category (plain, algo, order lists), then read the
    /// open orders again. `remaining` lists what is still there.
    pub async fn cancel_all<G: SendGate>(
        &self,
        gate: &G,
        risk: &dyn RiskHook,
        journal: &dyn UnknownJournal,
        confirm: ConfirmToken,
        table: &RulesTable,
    ) -> Result<BulkReport<OpenOrder>, BulkError> {
        self.cancel_all_via(
            gate,
            risk,
            journal,
            &Route::User(confirm.spend()),
            table,
            true,
        )
        .await
    }

    async fn cancel_all_via<G: SendGate>(
        &self,
        gate: &G,
        risk: &dyn RiskHook,
        journal: &dyn UnknownJournal,
        route: &Route,
        table: &RulesTable,
        reread: bool,
    ) -> Result<BulkReport<OpenOrder>, BulkError> {
        // Read with the budget kept for cancels. An OKX algo type that cannot be read does not
        // stop the cancel: the orders that were read are cancelled and the type is reported.
        let read = self.open_orders_for_cancel().await?;
        let open = read.orders;
        let mut by_symbol: BTreeMap<&str, Vec<&OpenOrder>> = BTreeMap::new();
        for o in &open {
            by_symbol.entry(o.symbol.as_str()).or_default().push(o);
        }
        let mut report = BulkReport::new();
        report.unknown_algo_types = read.unknown_algo_types;
        for (symbol, orders) in by_symbol {
            let Ok(fresh) = self.fresh(table, symbol) else {
                // No fresh rules for this symbol: it cannot be cancelled here, and the
                // re-read below will list it.
                report.failed += 1;
                continue;
            };
            let rules = fresh.rules();
            let mut requests: Vec<Result<Built, TradeError>> = Vec::new();
            match (self.venue, self.product) {
                (AccountVenue::Binance, AccountProduct::UsdM) => {
                    requests.push(Ok(Built::Binance(queries::futures_cancel_all_open(
                        rules.symbol(),
                    ))));
                    requests.push(Ok(Built::Binance(queries::futures_cancel_all_algo(
                        rules.symbol(),
                    ))));
                }
                (AccountVenue::Binance, AccountProduct::Spot) => {
                    // `DELETE openOrders` also cancels the legs of order lists.
                    requests.push(Ok(Built::Binance(queries::spot_cancel_all(rules.symbol()))));
                }
                (AccountVenue::Okx, _) => {
                    for o in orders.iter().filter(|o| o.kind == OpenKind::Plain) {
                        requests.push(
                            OrderRef::exchange(&o.id)
                                .and_then(|id| okx::cancel_order(rules, &id))
                                .map(|r| Built::Okx(Box::new(r))),
                        );
                    }
                    // Every algo type that `cancel-algos` takes. The others (trailing stop,
                    // iceberg, TWAP) are not tried: the re-read lists them as remaining.
                    for t in OkxAlgoType::ALL_PENDING
                        .into_iter()
                        .filter(|t| t.cancel_algos_supported())
                    {
                        let ids: Vec<&str> = orders
                            .iter()
                            .filter(|o| o.kind == OpenKind::Algo(Some(t)))
                            .map(|o| o.id.as_str())
                            .collect();
                        for chunk in ids.chunks(okx::MAX_CANCEL_ALGOS) {
                            requests.push(
                                chunk
                                    .iter()
                                    .map(|id| OrderRef::exchange(id))
                                    .collect::<Result<Vec<_>, _>>()
                                    .and_then(|c| okx::cancel_algos(rules, t, &c))
                                    .map(|r| Built::Okx(Box::new(r))),
                            );
                        }
                    }
                }
            }
            for built in requests {
                self.send_built(gate, risk, journal, route, built, &fresh, &mut report)
                    .await;
            }
        }
        // Condition 68: success only if a fresh read finds nothing. The automatic close skips
        // this read and makes one of its own at the very end (the algo-order read budget of
        // OKX is small), so its `remaining` stays empty.
        if reread {
            match self.open_orders().await {
                Ok(open) => report.remaining = open,
                Err(_) => report.reread_failed = true,
            }
        }
        Ok(report)
    }

    /// Close every position at market (condition 70), then read the positions again.
    /// Spot has no positions to close.
    pub async fn flatten<G: SendGate>(
        &self,
        gate: &G,
        risk: &dyn RiskHook,
        journal: &dyn UnknownJournal,
        confirm: ConfirmToken,
        table: &RulesTable,
        margin: MarginMode,
    ) -> Result<BulkReport<Position>, BulkError> {
        self.flatten_via(
            gate,
            risk,
            journal,
            &Route::User(confirm.spend()),
            table,
            margin,
            true,
        )
        .await
    }

    async fn flatten_via<G: SendGate>(
        &self,
        gate: &G,
        risk: &dyn RiskHook,
        journal: &dyn UnknownJournal,
        route: &Route,
        table: &RulesTable,
        margin: MarginMode,
        reread: bool,
    ) -> Result<BulkReport<Position>, BulkError> {
        if self.product == AccountProduct::Spot {
            return Err(TradeError::Unsupported.into());
        }
        let positions = self.positions().await?;
        let mut report = BulkReport::new();
        for p in &positions {
            let Ok(fresh) = self.fresh(table, &p.symbol) else {
                report.failed += 1;
                continue;
            };
            let rules = fresh.rules();
            let mode = Some(if p.hedge_side {
                PositionMode::Hedge
            } else {
                PositionMode::OneWay
            });
            let cid = ClientId::generate(Role::Entry);
            let built = match self.venue {
                AccountVenue::Binance => fut::new_order(fut::FuturesEntry {
                    rules,
                    mode,
                    action: Action::Close(p.dir),
                    qty: p.qty,
                    exec: Exec::Market,
                    ref_price: None,
                    client_id: cid,
                })
                .map(Built::Binance),
                AccountVenue::Okx => {
                    okx::close_position(rules, mode, p.margin.unwrap_or(margin), p.dir, cid)
                        .map(|r| Built::Okx(Box::new(r)))
                }
            };
            self.send_built(gate, risk, journal, route, built, &fresh, &mut report)
                .await;
        }
        if reread {
            match self.positions().await {
                Ok(positions) => report.remaining = positions,
                Err(_) => report.reread_failed = true,
            }
        }
        Ok(report)
    }

    /// The automatic risk close (condition 71, user decision 2): cancel everything, close
    /// every position, then read the account again. Needs an [`AutoCloseGrant`], which only a
    /// tripped, fresh, enabled `EquityTracker` hands out. Every request goes through the gate
    /// as an automatic-path intent, so anything that would add exposure is refused.
    ///
    /// The result is only reported after the final reads (conditions 68, 70); the reads
    /// inside `cancel` and `flatten` are skipped, so `remaining` there is empty. A failed step
    /// does not stop the next one: a failed cancel-all still flattens, and vice versa.
    pub async fn auto_close<G: SendGate>(
        &self,
        gate: &G,
        risk: &dyn RiskHook,
        journal: &dyn UnknownJournal,
        grant: AutoCloseGrant,
        table: &RulesTable,
        margin: MarginMode,
    ) -> AutoCloseReport {
        // A grant is for one account; it is not a licence to close another one.
        if grant.account != self.account {
            return AutoCloseReport::refused();
        }
        let cancel = self
            .cancel_all_via(gate, risk, journal, &Route::RiskClose, table, false)
            .await;
        let flatten = if self.product == AccountProduct::Spot {
            None
        } else {
            Some(
                self.flatten_via(gate, risk, journal, &Route::RiskClose, table, margin, false)
                    .await,
            )
        };
        // The report is built from reads made after every send.
        let orders = self.open_orders().await;
        let positions = if self.product == AccountProduct::Spot {
            Ok(Vec::new())
        } else {
            self.positions().await
        };
        AutoCloseReport {
            cancel,
            flatten,
            orders,
            positions,
        }
    }
}

/// What an automatic close did. `orders` and `positions` are fresh reads taken after all
/// sends; trust those, not the counts in `cancel` and `flatten`.
#[derive(Debug)]
pub struct AutoCloseReport {
    pub cancel: Result<BulkReport<OpenOrder>, BulkError>,
    /// `None` for spot, which has no positions to close.
    pub flatten: Option<Result<BulkReport<Position>, BulkError>>,
    pub orders: Result<Vec<OpenOrder>, ReadError>,
    pub positions: Result<Vec<Position>, ReadError>,
}

impl AutoCloseReport {
    fn refused() -> Self {
        Self {
            cancel: Err(BulkError::Trade(TradeError::Unsupported)),
            flatten: None,
            orders: Err(ReadError::Trade(TradeError::Unsupported)),
            positions: Err(ReadError::Trade(TradeError::Unsupported)),
        }
    }

    /// Both final reads worked and found no order and no position.
    pub fn is_clear(&self) -> bool {
        matches!(&self.orders, Ok(o) if o.is_empty())
            && matches!(&self.positions, Ok(p) if p.is_empty())
    }

    /// A final read failed, so the outcome is not known.
    pub fn reread_failed(&self) -> bool {
        self.orders.is_err() || self.positions.is_err()
    }

    /// Open orders of a type this app cannot cancel (OKX trailing stop, iceberg, TWAP):
    /// listed as remaining, never reported as cancelled (review item 15).
    pub fn uncancellable(&self) -> Vec<&OpenOrder> {
        match &self.orders {
            Ok(orders) => orders
                .iter()
                .filter(
                    |o| matches!(o.kind, OpenKind::Algo(Some(t)) if !t.cancel_algos_supported()),
                )
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    /// The notice for the GUI and the risk log, built from the final reads.
    pub fn notice(&self, account: crate::account::AccountKey, at_ms: i64) -> RiskNotice {
        RiskNotice {
            account,
            at_ms,
            kind: RiskNoticeKind::AutoCloseFinished {
                clear: self.is_clear(),
                remaining_orders: self.remaining_orders(),
                remaining_positions: self.remaining_positions(),
                reread_failed: self.reread_failed(),
            },
        }
    }

    /// `None` when the final read failed: the count is not known, which is not zero.
    pub fn remaining_orders(&self) -> Option<usize> {
        self.orders.as_ref().ok().map(Vec::len)
    }

    /// `None` when the final read failed.
    pub fn remaining_positions(&self) -> Option<usize> {
        self.positions.as_ref().ok().map(Vec::len)
    }
}
