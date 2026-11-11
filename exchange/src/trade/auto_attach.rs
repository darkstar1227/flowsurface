//! The protective orders that follow an entry fill on Binance USDⓈ-M (M5 d3): which orders, and
//! the request of each, worked out from the fill. Pure: nothing is sent here; the requests go
//! through `AutoGrant::authorise` and `TradeClient::submit_auto`.
//!
//! Two stages, because Binance conditional orders cannot be changed once placed:
//!
//! - [`Stage::Stop`]: at the **first fill**, a whole-position stop (`closePosition`), at a price
//!   worked out from the fill price. It covers whatever fills later, so a partly filled entry is
//!   protected at once and the stop never needs a replacement.
//! - [`Stage::Rest`]: when the **entry has finished** (filled, or cancelled with a partial
//!   fill), the take-profit orders (up to three, the last one taking what is left) and a
//!   trailing stop, sized from the total filled quantity at the average fill price.
//!
//! A hedge account gets whole-position market orders only (one take-profit, no trailing); the
//! builder refuses anything else there.

use super::binance_futures::new_algo_order;
use super::client_id::{ClientId, Role};
use super::decimal::Decimal;
use super::manual::entry_protection;
use super::protect::{Offset, ProtectPlan, ProtectSpec, TriggerBasis};
use super::request::BinanceRequest;
use super::rules::SymbolRules;
use super::types::{
    Dir, Protective, ProtectiveKind, Size, TimeInForce, TradeError, Trailing, WorkingType,
};
use crate::account::PositionMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// The whole-position stop-loss, at the first fill.
    Stop,
    /// Take-profit orders and the trailing stop, once the entry has finished.
    Rest,
}

/// The roles a stage sends, in order. Empty when the settings hold nothing for the stage.
pub fn stage_roles(spec: &ProtectSpec, stage: Stage) -> Vec<Role> {
    match stage {
        Stage::Stop => {
            if spec.stop.is_some() {
                vec![Role::StopLoss]
            } else {
                Vec::new()
            }
        }
        Stage::Rest => {
            let mut roles = vec![Role::TakeProfit; spec.targets.len()];
            if spec.trailing.is_some() {
                roles.push(Role::Trailing);
            }
            roles
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct StageInput<'a> {
    pub rules: &'a SymbolRules,
    pub mode: PositionMode,
    pub dir: Dir,
    /// Average price of the fills so far (the stop uses the first fill's).
    pub avg_price: Decimal,
    pub filled: Decimal,
    /// The market price now: the builder refuses a trigger the market is already past.
    pub ref_price: Decimal,
    pub spec: &'a ProtectSpec,
    pub stage: Stage,
}

impl StageInput<'_> {
    /// What the requests of this stage are based on: the entry they protect and the settings
    /// they were worked out from.
    pub fn basis(&self, entry_id: &str) -> super::grant::Basis {
        super::grant::Basis::new(entry_id, self.spec)
    }
}

fn stage_spec(spec: &ProtectSpec, stage: Stage) -> ProtectSpec {
    match stage {
        Stage::Stop => ProtectSpec {
            stop: spec.stop,
            targets: Vec::new(),
            trailing: None,
            trigger_price: spec.trigger_price,
        },
        Stage::Rest => ProtectSpec {
            stop: None,
            targets: spec.targets.clone(),
            trailing: spec.trailing,
            trigger_price: spec.trigger_price,
        },
    }
}

fn working_type(b: TriggerBasis) -> WorkingType {
    match b {
        TriggerBasis::Last => WorkingType::ContractPrice,
        TriggerBasis::Mark => WorkingType::MarkPrice,
    }
}

/// Where a trailing stop starts, from the average fill price: with the position, never against it.
fn activation(dir: Dir, avg: &Decimal, offset: Offset) -> Result<Decimal, TradeError> {
    let delta = match offset {
        Offset::Distance(d) => d,
        Offset::Percent(p) => avg
            .checked_mul(&p)
            .and_then(|v| v.checked_mul(&Decimal::parse("0.01")?))
            .map_err(|_| TradeError::ProtectionNotPossible)?,
    };
    let price = match dir {
        Dir::Long => avg.checked_add(&delta),
        Dir::Short => avg.checked_sub(&delta),
    }
    .map_err(|_| TradeError::ProtectionNotPossible)?;
    if price.is_positive() {
        Ok(price)
    } else {
        Err(TradeError::ProtectionNotPossible)
    }
}

/// One request per id in `ids`, which must be the roles of [`stage_roles`] in the same order
/// (one id each: an id belongs to one request). When the prices or quantities cannot be worked
/// out, every request of the stage is an error: nothing is half sent.
pub fn build_stage(
    input: &StageInput<'_>,
    ids: Vec<(Role, ClientId)>,
) -> Vec<(Role, Result<BinanceRequest, TradeError>)> {
    let spec = stage_spec(input.spec, input.stage);
    let wanted = stage_roles(input.spec, input.stage);
    let plan = if ids.iter().map(|(r, _)| *r).eq(wanted.iter().copied()) {
        entry_protection(
            input.rules,
            input.dir,
            &input.avg_price,
            &input.filled,
            &spec,
        )
    } else {
        Err(TradeError::ProtectionNotPossible)
    };
    let mut targets = plan
        .as_ref()
        .map(|p: &ProtectPlan| p.targets.clone())
        .unwrap_or_default()
        .into_iter();
    ids.into_iter()
        .map(|(role, id)| {
            let built = plan
                .as_ref()
                .map_err(|e| *e)
                .and_then(|p| build_one(input, p, role, &mut targets, id));
            (role, built)
        })
        .collect()
}

fn build_one(
    input: &StageInput<'_>,
    plan: &ProtectPlan,
    role: Role,
    targets: &mut impl Iterator<Item = super::protect::TargetLeg>,
    client_id: ClientId,
) -> Result<BinanceRequest, TradeError> {
    let hedge = input.mode == PositionMode::Hedge;
    let working = working_type(input.spec.trigger_price);
    let base = Protective {
        kind: ProtectiveKind::StopMarket,
        dir: input.dir,
        size: Size::ClosePosition,
        trigger_price: None,
        limit_price: None,
        tif: TimeInForce::Gtc,
        working_type: working,
        trailing: None,
        ref_price: input.ref_price,
        client_id,
    };
    let p = match role {
        Role::StopLoss => Protective {
            trigger_price: Some(plan.stop.ok_or(TradeError::ProtectionNotPossible)?),
            ..base
        },
        Role::TakeProfit => {
            let leg = targets.next().ok_or(TradeError::ProtectionNotPossible)?;
            Protective {
                kind: ProtectiveKind::TakeProfitMarket,
                size: if hedge {
                    Size::ClosePosition
                } else {
                    Size::Qty(leg.qty)
                },
                trigger_price: Some(leg.trigger),
                ..base
            }
        }
        Role::Trailing => {
            let t = input
                .spec
                .trailing
                .ok_or(TradeError::ProtectionNotPossible)?;
            let activate_price = t
                .activation
                .map(|o| activation(input.dir, &input.avg_price, o))
                .transpose()?;
            Protective {
                kind: ProtectiveKind::Trailing,
                size: if hedge {
                    Size::ClosePosition
                } else {
                    Size::Qty(plan.total_qty)
                },
                trailing: Some(Trailing::CallbackRate {
                    percent: t.callback_pct,
                    activate_price,
                }),
                ..base
            }
        }
        Role::Entry => return Err(TradeError::ProtectionNotPossible),
    };
    new_algo_order(input.rules, Some(input.mode), p)
}

#[cfg(test)]
mod tests;
