//! Helpers shared by the venue builders.

use super::client_id::IdField;
use super::decimal::Decimal;
use super::request::BinanceRequest;
use super::types::{Dir, OrderRef, ProtectiveKind, TradeError};
use std::cmp::Ordering;

/// Add `orderId`-style or client-id parameter for a cancel or query request.
pub(crate) fn push_ref(
    req: &mut BinanceRequest,
    order: &OrderRef,
    exchange_name: &'static str,
    client_name: &'static str,
    field: IdField,
) -> Result<(), TradeError> {
    match order {
        OrderRef::Exchange(id) => {
            // Built only through `OrderRef::exchange`, but re-check: it is cheap.
            OrderRef::exchange(id)?;
            req.push(exchange_name, id.clone());
        }
        OrderRef::Client(id) => req.push(client_name, id.check(field)?.to_string()),
    }
    Ok(())
}

/// Which side of the market a trigger must be on so it does not fire immediately.
/// A long position's stop is below the market and its take-profit above; a short is the
/// reverse. `trigger` and `ref_price` must be positive.
pub(crate) fn check_trigger_side(
    kind: ProtectiveKind,
    dir: Dir,
    trigger: &Decimal,
    ref_price: &Decimal,
) -> Result<(), TradeError> {
    if !ref_price.is_positive() {
        return Err(TradeError::MissingReferencePrice);
    }
    let below_market = matches!(
        (kind.is_stop_side(), dir),
        (true, Dir::Long) | (false, Dir::Short)
    );
    let ord = trigger.checked_cmp(ref_price)?;
    let ok = if below_market {
        ord == Ordering::Less
    } else {
        ord == Ordering::Greater
    };
    if ok {
        Ok(())
    } else {
        Err(TradeError::TriggerOnWrongSide)
    }
}
