//! Cancel-all (condition 68) and flatten (condition 70). Both go through the single gate for
//! every request, then read the account again: success is only reported when nothing is left.
//! A request that cannot be built or is refused is counted as failed and the rest still run;
//! the re-read at the end always happens, so the report shows what really remains.

use super::binance_futures as fut;
use super::client::{OpenKind, OpenOrder, Position, ReadError, TradeClient};
use super::client_id::{ClientId, Role};
use super::gate::{ConfirmToken, OrderIntent, RiskHook, SendGate, Spent};
use super::okx::{self, OkxAlgoType};
use super::outcome::Outcome;
use super::queries;
use super::request::{BinanceRequest, OkxRequest};
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
}

impl<T> BulkReport<T> {
    fn new() -> Self {
        Self {
            sent: 0,
            failed: 0,
            remaining: Vec::new(),
            reread_failed: false,
            pending: Vec::new(),
        }
    }

    /// Nothing is left, nothing failed, and the final read worked.
    pub fn is_clear(&self) -> bool {
        !self.reread_failed && self.remaining.is_empty() && self.failed == 0
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
        spent: &Spent,
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
        match self.submit_spent(gate, risk, spent.clone(), intent).await {
            Ok(sub) => {
                report.sent += 1;
                if !matches!(sub.outcome, Outcome::Accepted { .. }) {
                    report.failed += 1;
                }
                report.pending.extend(sub.pending);
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
        confirm: ConfirmToken,
        table: &RulesTable,
    ) -> Result<BulkReport<OpenOrder>, BulkError> {
        let spent = confirm.spend();
        let open = self.open_orders().await?;
        let mut by_symbol: BTreeMap<&str, Vec<&OpenOrder>> = BTreeMap::new();
        for o in &open {
            by_symbol.entry(o.symbol.as_str()).or_default().push(o);
        }
        let mut report = BulkReport::new();
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
                    for t in [OkxAlgoType::Conditional, OkxAlgoType::Oco] {
                        let ids: Vec<Result<OrderRef, TradeError>> = orders
                            .iter()
                            .filter(|o| o.kind == OpenKind::Algo(Some(t)))
                            .map(|o| OrderRef::exchange(&o.id))
                            .collect();
                        for chunk in ids.chunks(okx::MAX_CANCEL_ALGOS) {
                            requests.push(
                                chunk
                                    .iter()
                                    .cloned()
                                    .collect::<Result<Vec<_>, _>>()
                                    .and_then(|c| okx::cancel_algos(rules, t, &c))
                                    .map(|r| Built::Okx(Box::new(r))),
                            );
                        }
                    }
                }
            }
            for built in requests {
                self.send_built(gate, risk, &spent, built, &fresh, &mut report)
                    .await;
            }
        }
        // Condition 68: success only if a fresh read finds nothing.
        match self.open_orders().await {
            Ok(open) => report.remaining = open,
            Err(_) => report.reread_failed = true,
        }
        Ok(report)
    }

    /// Close every position at market (condition 70), then read the positions again.
    /// Spot has no positions to close.
    pub async fn flatten<G: SendGate>(
        &self,
        gate: &G,
        risk: &dyn RiskHook,
        confirm: ConfirmToken,
        table: &RulesTable,
        margin: MarginMode,
    ) -> Result<BulkReport<Position>, BulkError> {
        if self.product == AccountProduct::Spot {
            return Err(TradeError::Unsupported.into());
        }
        let spent = confirm.spend();
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
                AccountVenue::Binance => fut::new_order(&fut::FuturesEntry {
                    rules,
                    mode,
                    action: Action::Close(p.dir),
                    qty: p.qty,
                    exec: Exec::Market,
                    ref_price: None,
                    client_id: &cid,
                })
                .map(Built::Binance),
                AccountVenue::Okx => {
                    okx::close_position(rules, mode, p.margin.unwrap_or(margin), p.dir, &cid)
                        .map(|r| Built::Okx(Box::new(r)))
                }
            };
            self.send_built(gate, risk, &spent, built, &fresh, &mut report)
                .await;
        }
        match self.positions().await {
            Ok(positions) => report.remaining = positions,
            Err(_) => report.reread_failed = true,
        }
        Ok(report)
    }
}
