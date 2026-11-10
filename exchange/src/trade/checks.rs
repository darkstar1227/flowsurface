//! Limit checks (condition 63) and OKX contract conversions. Pure functions on decimals.

use super::decimal::Decimal;
use super::rules::{OkxContractType, SymbolRules};
use super::types::TradeError;
use std::cmp::Ordering;

/// Default fat-finger threshold: 5% (500 basis points). The confirmation UI (stage c) uses it.
pub const DEFAULT_DEVIATION_BPS: u32 = 500;

/// Check a rounded quantity against minQty / maxQty (`MARKET_LOT_SIZE` for market orders).
pub fn check_qty(rules: &SymbolRules, qty: &Decimal, market: bool) -> Result<(), TradeError> {
    let rule = rules.qty_rule(market);
    if qty.checked_cmp(&rule.min)? == Ordering::Less {
        return Err(TradeError::QtyBelowMin);
    }
    if let Some(max) = &rule.max
        && qty.checked_cmp(max)? == Ordering::Greater
    {
        return Err(TradeError::QtyAboveMax);
    }
    Ok(())
}

/// Order value in the quote currency (OKX swaps: see [`okx_notional`]).
pub fn notional(
    rules: &SymbolRules,
    qty: &Decimal,
    price: &Decimal,
) -> Result<Decimal, TradeError> {
    match rules.okx {
        Some(contract) => okx_notional(qty, &contract.ct_val, contract.ct_type, price),
        None => Ok(qty.checked_mul(price)?),
    }
}

/// Check the order value against the minimum and maximum notional. `price` is the limit
/// price, or the reference price for market orders (the exchange uses its own average
/// price; a nearby reference is the best the client can do, and the exchange re-checks).
pub fn check_notional(
    rules: &SymbolRules,
    qty: &Decimal,
    price: &Decimal,
    market: bool,
) -> Result<(), TradeError> {
    let Some(rule) = &rules.notional else {
        return Ok(());
    };
    let value = notional(rules, qty, price)?;
    if let Some(min) = &rule.min
        && (!market || rule.min_applies_to_market)
        && value.checked_cmp(min)? == Ordering::Less
    {
        return Err(TradeError::NotionalBelowMin);
    }
    if let Some(max) = &rule.max
        && if market {
            rule.max_applies_to_market
        } else {
            rule.max_applies_to_limit
        }
        && value.checked_cmp(max)? == Ordering::Greater
    {
        return Err(TradeError::NotionalAboveMax);
    }
    Ok(())
}

/// Both quantity and notional checks for an order about to be built.
pub(crate) fn check_order(
    rules: &SymbolRules,
    qty: &Decimal,
    price: Option<&Decimal>,
    market: bool,
) -> Result<Option<Decimal>, TradeError> {
    check_qty(rules, qty, market)?;
    if rules.notional.is_some() {
        let price = price.ok_or(TradeError::MissingReferencePrice)?;
        check_notional(rules, qty, price, market)?;
    }
    // The order value, for the single-order cap; `None` when no price is known.
    price.map(|p| notional(rules, qty, p)).transpose()
}

/// `|price - reference| / reference > threshold_bps / 10000`, in exact arithmetic.
/// Equal to the threshold is not an excess.
pub fn deviation_exceeds(
    price: &Decimal,
    reference: &Decimal,
    threshold_bps: u32,
) -> Result<bool, TradeError> {
    if !reference.is_positive() || !price.is_positive() {
        return Err(TradeError::NonPositive);
    }
    let diff = price.checked_sub(reference)?.checked_abs()?;
    let lhs = diff.checked_mul(&Decimal::from_int(10_000))?;
    let rhs = reference.checked_mul(&Decimal::from_int(i64::from(threshold_bps)))?;
    Ok(lhs.checked_cmp(&rhs)? == Ordering::Greater)
}

/// Base-currency amount held by `contracts` contracts.
pub fn contracts_to_base(contracts: &Decimal, ct_val: &Decimal) -> Result<Decimal, TradeError> {
    Ok(contracts.checked_mul(ct_val)?)
}

/// Whole lots of `lot_sz` contracts that fit in a base-currency amount (towards zero).
/// Zero contracts is an error.
pub fn base_to_contracts(
    base: &Decimal,
    ct_val: &Decimal,
    lot_sz: &Decimal,
) -> Result<Decimal, TradeError> {
    if !base.is_positive() || !ct_val.is_positive() {
        return Err(TradeError::NonPositive);
    }
    // Whole lots of `ct_val * lot_sz` base units, then back to contracts.
    let lot_base = ct_val.checked_mul(lot_sz)?;
    let lots = base.whole_steps(&lot_base)?;
    if lots <= 0 {
        return Err(TradeError::NonPositive);
    }
    Ok(Decimal::from_i128(lots)?.checked_mul(lot_sz)?)
}

/// Order value of an OKX swap order: linear `sz * ctVal * px` in the quote currency,
/// inverse `sz * ctVal` in USD (the price drops out).
pub fn okx_notional(
    contracts: &Decimal,
    ct_val: &Decimal,
    ct_type: OkxContractType,
    price: &Decimal,
) -> Result<Decimal, TradeError> {
    let base = contracts.checked_mul(ct_val)?;
    match ct_type {
        OkxContractType::Linear => Ok(base.checked_mul(price)?),
        OkxContractType::Inverse => Ok(base),
    }
}

/// A protective spot sell must not be larger than what the account can sell after fees
/// (condition 64). `available` is the free balance of the base asset, fee already deducted
/// by the caller.
pub fn check_protective_qty_available(
    qty: &Decimal,
    available: &Decimal,
) -> Result<(), TradeError> {
    if qty.checked_cmp(available)? == Ordering::Greater {
        Err(TradeError::ExceedsAvailable)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::rules::fixtures::*;
    use super::*;

    #[test]
    fn quantity_limits_use_market_lot_size_for_market_orders() {
        let t = spot();
        let r = t.lookup("BTCUSDT").unwrap();
        assert_eq!(
            check_qty(r, &d("0.000001"), false),
            Err(TradeError::QtyBelowMin)
        );
        assert!(check_qty(r, &d("0.00001"), false).is_ok());
        assert_eq!(
            check_qty(r, &d("9001"), false),
            Err(TradeError::QtyAboveMax)
        );
        // 500 is fine for a limit order but over MARKET_LOT_SIZE's 100
        assert!(check_qty(r, &d("500"), false).is_ok());
        assert_eq!(check_qty(r, &d("500"), true), Err(TradeError::QtyAboveMax));
    }

    #[test]
    fn min_notional_applies_to_market_only_when_flagged() {
        let t = spot();
        let r = t.lookup("BTCUSDT").unwrap();
        // 0.0001 * 100 = 0.01 < 5
        assert_eq!(
            check_notional(r, &d("0.0001"), &d("100"), false),
            Err(TradeError::NotionalBelowMin)
        );
        assert_eq!(
            check_notional(r, &d("0.0001"), &d("100"), true),
            Err(TradeError::NotionalBelowMin)
        );
        assert!(check_notional(r, &d("0.1"), &d("100"), false).is_ok());
        // maxNotional 9,000,000 does not apply to market orders (applyMaxToMarket=false)
        assert!(check_notional(r, &d("90"), &d("1000000"), true).is_ok());
        assert_eq!(
            check_notional(r, &d("90"), &d("1000000"), false),
            Err(TradeError::NotionalAboveMax)
        );
    }

    #[test]
    fn futures_min_notional() {
        let t = futures();
        let r = t.lookup("BTCUSDT").unwrap();
        assert_eq!(
            check_notional(r, &d("0.001"), &d("60000"), false),
            Err(TradeError::NotionalBelowMin)
        );
        assert!(check_notional(r, &d("0.002"), &d("60000"), false).is_ok());
    }

    #[test]
    fn deviation_threshold_is_exact() {
        // 5% of 100 is 5: 105 is equal (not above), 105.01 is above
        assert!(!deviation_exceeds(&d("105"), &d("100"), DEFAULT_DEVIATION_BPS).unwrap());
        assert!(deviation_exceeds(&d("105.01"), &d("100"), DEFAULT_DEVIATION_BPS).unwrap());
        assert!(!deviation_exceeds(&d("95"), &d("100"), DEFAULT_DEVIATION_BPS).unwrap());
        assert!(deviation_exceeds(&d("94.99"), &d("100"), DEFAULT_DEVIATION_BPS).unwrap());
        assert!(deviation_exceeds(&d("1"), &d("0"), 500).is_err());
    }

    #[test]
    fn okx_conversions() {
        // 3 contracts of 0.01 BTC = 0.03 BTC
        assert_eq!(contracts_to_base(&d("3"), &d("0.01")).unwrap(), d("0.03"));
        // 0.035 BTC fits 3 whole contracts (towards zero)
        assert_eq!(
            base_to_contracts(&d("0.035"), &d("0.01"), &d("1")).unwrap(),
            d("3")
        );
        // lot of 5 contracts: 0.14 BTC = 14 contracts -> 10
        assert_eq!(
            base_to_contracts(&d("0.14"), &d("0.01"), &d("5")).unwrap(),
            d("10")
        );
        assert!(base_to_contracts(&d("0.005"), &d("0.01"), &d("1")).is_err());
        // linear: 3 * 0.01 * 60000 = 1800 USDT ; inverse: 3 * 100 = 300 USD
        assert_eq!(
            okx_notional(&d("3"), &d("0.01"), OkxContractType::Linear, &d("60000")).unwrap(),
            d("1800")
        );
        assert_eq!(
            okx_notional(&d("3"), &d("100"), OkxContractType::Inverse, &d("60000")).unwrap(),
            d("300")
        );
    }

    #[test]
    fn okx_notional_uses_contract_value() {
        let t = okx_swap_table();
        let r = t.lookup("BTC-USDT-SWAP").unwrap();
        assert_eq!(notional(r, &d("3"), &d("60000")).unwrap(), d("1800"));
        let inv = t.lookup("BTC-USD-SWAP").unwrap();
        assert_eq!(notional(inv, &d("3"), &d("60000")).unwrap(), d("300"));
    }

    #[test]
    fn protective_quantity_cannot_exceed_available() {
        assert!(check_protective_qty_available(&d("0.5"), &d("0.5")).is_ok());
        assert_eq!(
            check_protective_qty_available(&d("0.51"), &d("0.5")),
            Err(TradeError::ExceedsAvailable)
        );
    }
}
