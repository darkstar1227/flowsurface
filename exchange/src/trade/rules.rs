//! Trading rules parsed from the exchanges' raw strings (condition 62).
//!
//! The existing `MinTicksize` / `MinQtySize` round to powers of ten and are display
//! types: they must not be used for orders. Rules here keep the exact tickSize, stepSize,
//! limits and notional strings as fixed-point decimals. A symbol whose rules are missing,
//! zero where a step is needed, or too precise to hold is left out of the table, so it
//! cannot be traded.

use super::decimal::Decimal;
use super::types::{Side, TradeError};
use crate::signing::validate_symbol;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt;

/// The four products the trade module can build orders for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TradeProduct {
    BinanceSpot,
    BinanceUsdM,
    OkxSpot,
    OkxSwap,
}

/// A symbol / instId that exists in a [`RulesTable`] and has only `[A-Z0-9-]` characters
/// (condition 44). The field is private and there is no `From<&str>`, `Default` or
/// `Deserialize`: the only constructor is [`RulesTable::validate`], and builders take
/// their symbol from the [`SymbolRules`] that owns one.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ValidSymbol(String);

impl ValidSymbol {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ValidSymbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ValidSymbol({})", self.0)
    }
}

impl fmt::Display for ValidSymbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// `min <= qty <= max`, with a step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QtyRule {
    pub min: Decimal,
    pub max: Option<Decimal>,
    pub step: Decimal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotionalRule {
    pub min: Option<Decimal>,
    pub max: Option<Decimal>,
    pub min_applies_to_market: bool,
    pub max_applies_to_market: bool,
    /// False for OKX spot, whose `maxMktSz` is a market-order notional only.
    pub max_applies_to_limit: bool,
}

/// Spot `TRAILING_DELTA` filter, in basis points.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrailingDeltaRule {
    pub min_above: u32,
    pub max_above: u32,
    pub min_below: u32,
    pub max_below: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OkxContractType {
    Linear,
    Inverse,
}

/// OKX contract facts: `sz` counts contracts of `ct_val` base units each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OkxContract {
    pub ct_val: Decimal,
    pub ct_type: OkxContractType,
}

#[derive(Debug, Clone)]
pub struct SymbolRules {
    pub(crate) product: TradeProduct,
    pub(crate) symbol: ValidSymbol,
    pub(crate) tick: Decimal,
    /// `LOT_SIZE` / `lotSz`, `minSz`.
    pub(crate) lot: QtyRule,
    /// `MARKET_LOT_SIZE` for Binance; OKX swap `maxMktSz` (contracts) goes in `max`. OKX
    /// spot `maxMktSz` is a USDT amount and lives in `notional`.
    pub(crate) market_lot: Option<QtyRule>,
    pub(crate) notional: Option<NotionalRule>,
    pub(crate) trailing_delta: Option<TrailingDeltaRule>,
    pub(crate) okx: Option<OkxContract>,
}

impl SymbolRules {
    pub fn product(&self) -> TradeProduct {
        self.product
    }

    pub fn symbol(&self) -> &ValidSymbol {
        &self.symbol
    }

    pub fn tick(&self) -> &Decimal {
        &self.tick
    }

    pub fn lot(&self) -> &QtyRule {
        &self.lot
    }

    pub fn market_lot(&self) -> Option<&QtyRule> {
        self.market_lot.as_ref()
    }

    pub fn okx_contract(&self) -> Option<&OkxContract> {
        self.okx.as_ref()
    }

    pub(crate) fn require_product(&self, product: TradeProduct) -> Result<(), TradeError> {
        if self.product == product {
            Ok(())
        } else {
            Err(TradeError::ProductMismatch)
        }
    }

    /// The rule that applies to this execution style.
    pub(crate) fn qty_rule(&self, market: bool) -> &QtyRule {
        match (market, &self.market_lot) {
            (true, Some(r)) => r,
            _ => &self.lot,
        }
    }

    /// Round a quantity towards zero to the step. Rounding to zero is an error.
    pub fn round_qty(&self, qty: &Decimal, market: bool) -> Result<Decimal, TradeError> {
        if !qty.is_positive() {
            return Err(TradeError::NonPositive);
        }
        let rounded = qty.trunc_to_step(&self.qty_rule(market).step)?;
        if rounded.is_positive() {
            Ok(rounded)
        } else {
            Err(TradeError::NonPositive)
        }
    }

    /// Round a limit price so it never crosses the user's price: buys down, sells up.
    pub fn round_limit_price(&self, side: Side, price: &Decimal) -> Result<Decimal, TradeError> {
        if !price.is_positive() {
            return Err(TradeError::NonPositive);
        }
        let rounded = match side {
            Side::Buy => price.floor_to_step(&self.tick)?,
            Side::Sell => price.ceil_to_step(&self.tick)?,
        };
        if rounded.is_positive() {
            Ok(rounded)
        } else {
            Err(TradeError::NonPositive)
        }
    }

    /// Round a trigger price to the nearest tick (ties go up).
    pub fn round_trigger_price(&self, price: &Decimal) -> Result<Decimal, TradeError> {
        if !price.is_positive() {
            return Err(TradeError::NonPositive);
        }
        let rounded = price.round_to_step(&self.tick)?;
        if rounded.is_positive() {
            Ok(rounded)
        } else {
            Err(TradeError::NonPositive)
        }
    }
}

/// All tradable symbols of one product, with the time the rules were fetched.
#[derive(Debug, Clone)]
pub struct RulesTable {
    product: TradeProduct,
    fetched_at_ms: i64,
    symbols: BTreeMap<String, SymbolRules>,
    /// Symbols left out because their rules were missing or unusable.
    skipped: Vec<String>,
}

impl RulesTable {
    pub fn product(&self) -> TradeProduct {
        self.product
    }

    pub fn len(&self) -> usize {
        self.symbols.len()
    }

    pub fn is_empty(&self) -> bool {
        self.symbols.is_empty()
    }

    pub fn skipped(&self) -> &[String] {
        &self.skipped
    }

    /// Reject rules older than `max_age_ms` (condition 62).
    pub fn check_fresh(&self, now_ms: i64, max_age_ms: i64) -> Result<(), TradeError> {
        match now_ms.checked_sub(self.fetched_at_ms) {
            Some(age) if (0..=max_age_ms).contains(&age) => Ok(()),
            _ => Err(TradeError::RulesStale),
        }
    }

    /// The only way to get a [`ValidSymbol`] (condition 44).
    pub fn validate(&self, raw: &str) -> Result<ValidSymbol, TradeError> {
        Ok(self.lookup(raw)?.symbol.clone())
    }

    pub fn lookup(&self, raw: &str) -> Result<&SymbolRules, TradeError> {
        let checked = validate_symbol(raw)?;
        self.symbols.get(checked).ok_or(TradeError::UnknownSymbol)
    }

    /// Parse a Binance `exchangeInfo` response (spot or USDⓈ-M).
    pub fn from_binance_exchange_info(
        product: TradeProduct,
        info: &Value,
        fetched_at_ms: i64,
    ) -> Result<RulesTable, TradeError> {
        if !matches!(
            product,
            TradeProduct::BinanceSpot | TradeProduct::BinanceUsdM
        ) {
            return Err(TradeError::ProductMismatch);
        }
        let list = info["symbols"].as_array().ok_or(TradeError::RuleMissing)?;
        let mut table = RulesTable::new(product, fetched_at_ms);
        for item in list {
            let name = item["symbol"].as_str().unwrap_or_default();
            if item["status"].as_str() != Some("TRADING") {
                continue;
            }
            // Perpetual futures only: delivery symbols carry `_` and fail the check.
            if product == TradeProduct::BinanceUsdM
                && item["contractType"].as_str() != Some("PERPETUAL")
            {
                continue;
            }
            if validate_symbol(name).is_err() {
                continue;
            }
            match parse_binance_symbol(product, name, item) {
                Ok(rules) => {
                    table.symbols.insert(name.to_string(), rules);
                }
                Err(_) => table.skipped.push(name.to_string()),
            }
        }
        Ok(table)
    }

    /// Parse an OKX `public/instruments` response (SPOT or SWAP).
    pub fn from_okx_instruments(
        product: TradeProduct,
        resp: &Value,
        fetched_at_ms: i64,
    ) -> Result<RulesTable, TradeError> {
        if !matches!(product, TradeProduct::OkxSpot | TradeProduct::OkxSwap) {
            return Err(TradeError::ProductMismatch);
        }
        let list = resp["data"].as_array().ok_or(TradeError::RuleMissing)?;
        let mut table = RulesTable::new(product, fetched_at_ms);
        for item in list {
            let name = item["instId"].as_str().unwrap_or_default();
            if item["state"].as_str() != Some("live") {
                continue;
            }
            let want = match product {
                TradeProduct::OkxSpot => "SPOT",
                _ => "SWAP",
            };
            if item["instType"].as_str() != Some(want) || validate_symbol(name).is_err() {
                continue;
            }
            match parse_okx_instrument(product, name, item) {
                Ok(rules) => {
                    table.symbols.insert(name.to_string(), rules);
                }
                Err(_) => table.skipped.push(name.to_string()),
            }
        }
        Ok(table)
    }

    fn new(product: TradeProduct, fetched_at_ms: i64) -> RulesTable {
        RulesTable {
            product,
            fetched_at_ms,
            symbols: BTreeMap::new(),
            skipped: Vec::new(),
        }
    }
}

/// A decimal field that must be a JSON string. JSON numbers are refused so a value never
/// passes through `f64`.
fn dec(v: &Value) -> Result<Decimal, TradeError> {
    Ok(Decimal::parse(v.as_str().ok_or(TradeError::RuleMissing)?)?)
}

fn dec_opt(v: &Value) -> Result<Option<Decimal>, TradeError> {
    match v {
        Value::Null => Ok(None),
        other => dec(other).map(Some),
    }
}

/// Positive step (zero steps would divide by zero and mean "no rule": refuse).
fn step(v: &Value) -> Result<Decimal, TradeError> {
    let d = dec(v)?;
    if d.is_positive() {
        Ok(d)
    } else {
        Err(TradeError::RuleMissing)
    }
}

fn int_u32(v: &Value) -> Result<u32, TradeError> {
    v.as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .ok_or(TradeError::RuleMissing)
}

fn filter<'a>(filters: &'a [Value], name: &str) -> Option<&'a Value> {
    filters
        .iter()
        .find(|f| f["filterType"].as_str() == Some(name))
}

/// `zero_max_is_unlimited` is only true for MARKET_LOT_SIZE, where a `maxQty` of 0 means
/// the rule is disabled on some symbols. For LOT_SIZE a zero maximum is not a usable rule.
fn qty_rule(f: &Value, zero_max_is_unlimited: bool) -> Result<QtyRule, TradeError> {
    let max = dec(&f["maxQty"])?;
    let max = if max.is_zero() {
        if !zero_max_is_unlimited {
            return Err(TradeError::RuleMissing);
        }
        None
    } else {
        Some(max)
    };
    Ok(QtyRule {
        min: dec(&f["minQty"])?,
        max,
        step: step(&f["stepSize"])?,
    })
}

fn parse_binance_symbol(
    product: TradeProduct,
    name: &str,
    item: &Value,
) -> Result<SymbolRules, TradeError> {
    let filters = item["filters"].as_array().ok_or(TradeError::RuleMissing)?;
    let price = filter(filters, "PRICE_FILTER").ok_or(TradeError::RuleMissing)?;
    let lot = qty_rule(
        filter(filters, "LOT_SIZE").ok_or(TradeError::RuleMissing)?,
        false,
    )?;
    let market_lot = match filter(filters, "MARKET_LOT_SIZE") {
        Some(f) => Some(qty_rule(f, true)?),
        None => None,
    };

    let notional = if let Some(f) = filter(filters, "NOTIONAL") {
        Some(NotionalRule {
            // A NOTIONAL filter without a minimum is not usable: fail closed.
            min: Some(dec(&f["minNotional"])?),
            max: dec_opt(&f["maxNotional"])?,
            min_applies_to_market: f["applyMinToMarket"].as_bool().unwrap_or(true),
            max_applies_to_market: f["applyMaxToMarket"].as_bool().unwrap_or(true),
            max_applies_to_limit: true,
        })
    } else if let Some(f) = filter(filters, "MIN_NOTIONAL") {
        // Spot spells it `minNotional`; USDⓈ-M uses `notional` (未核對: not in the docs
        // copy). Either name is accepted, and a missing value fails closed.
        let min = if f.get("minNotional").is_some() {
            dec(&f["minNotional"])?
        } else {
            dec(&f["notional"])?
        };
        Some(NotionalRule {
            min: Some(min),
            max: None,
            min_applies_to_market: f["applyToMarket"].as_bool().unwrap_or(true),
            max_applies_to_market: false,
            max_applies_to_limit: true,
        })
    } else {
        // No minimum notional is a missing rule, not "no limit" (fail closed).
        return Err(TradeError::RuleMissing);
    };

    let trailing_delta = match filter(filters, "TRAILING_DELTA") {
        Some(f) => Some(TrailingDeltaRule {
            min_above: int_u32(&f["minTrailingAboveDelta"])?,
            max_above: int_u32(&f["maxTrailingAboveDelta"])?,
            min_below: int_u32(&f["minTrailingBelowDelta"])?,
            max_below: int_u32(&f["maxTrailingBelowDelta"])?,
        }),
        None => None,
    };

    Ok(SymbolRules {
        product,
        symbol: ValidSymbol(name.to_string()),
        tick: step(&price["tickSize"])?,
        lot,
        market_lot,
        notional,
        trailing_delta,
        okx: None,
    })
}

fn parse_okx_instrument(
    product: TradeProduct,
    name: &str,
    item: &Value,
) -> Result<SymbolRules, TradeError> {
    let step_sz = step(&item["lotSz"])?;
    let min = dec(&item["minSz"])?;
    // A missing limit field means no limit; a malformed one makes the symbol unusable.
    let max_lmt = dec_opt(&item["maxLmtSz"])?;
    let max_mkt = dec_opt(&item["maxMktSz"])?;
    let okx = if product == TradeProduct::OkxSwap {
        let ct_type = match item["ctType"].as_str() {
            Some("linear") => OkxContractType::Linear,
            Some("inverse") => OkxContractType::Inverse,
            _ => return Err(TradeError::RuleMissing),
        };
        Some(OkxContract {
            ct_val: step(&item["ctVal"])?,
            ct_type,
        })
    } else {
        None
    };
    Ok(SymbolRules {
        product,
        symbol: ValidSymbol(name.to_string()),
        tick: step(&item["tickSz"])?,
        lot: QtyRule {
            min,
            max: max_lmt,
            step: step_sz,
        },
        // Swap: `maxMktSz` counts contracts. Spot: it is a USDT amount (docs: for SPOT
        // the unit is USDT), so it becomes a market notional
        // cap and the market quantity has no separate base maximum.
        market_lot: match (product, max_mkt) {
            (TradeProduct::OkxSpot, _) => Some(QtyRule {
                min,
                max: None,
                step: step_sz,
            }),
            (_, Some(max)) => Some(QtyRule {
                min,
                max: Some(max),
                step: step_sz,
            }),
            (_, None) => None,
        },
        notional: match (product, max_mkt) {
            (TradeProduct::OkxSpot, Some(max)) => Some(NotionalRule {
                min: None,
                max: Some(max),
                min_applies_to_market: false,
                max_applies_to_market: true,
                max_applies_to_limit: false,
            }),
            _ => None,
        },
        trailing_delta: None,
        okx,
    })
}

#[cfg(test)]
pub(crate) mod fixtures {
    //! Rules parsed from the official documents' example shapes. The Binance spot filters
    //! are the examples in `filters.md`; the OKX instrument is the example in the
    //! `public/instruments` section. Values that differ are marked.
    use super::*;
    use serde_json::json;

    pub fn spot_info() -> Value {
        json!({"symbols": [
            {"symbol": "BTCUSDT", "status": "TRADING", "filters": [
                {"filterType": "PRICE_FILTER", "minPrice": "0.01000000", "maxPrice": "1000000.00000000", "tickSize": "0.01000000"},
                {"filterType": "LOT_SIZE", "minQty": "0.00001000", "maxQty": "9000.00000000", "stepSize": "0.00001000"},
                {"filterType": "MARKET_LOT_SIZE", "minQty": "0.00000000", "maxQty": "100.00000000", "stepSize": "0.00001000"},
                {"filterType": "NOTIONAL", "minNotional": "5.00000000", "applyMinToMarket": true, "maxNotional": "9000000.00000000", "applyMaxToMarket": false, "avgPriceMins": 5},
                {"filterType": "TRAILING_DELTA", "minTrailingAboveDelta": 10, "maxTrailingAboveDelta": 2000, "minTrailingBelowDelta": 10, "maxTrailingBelowDelta": 2000}
            ]},
            // Odd tick, to show the table does not round it to a power of ten.
            {"symbol": "ODDUSDT", "status": "TRADING", "filters": [
                {"filterType": "PRICE_FILTER", "minPrice": "0.25", "maxPrice": "0", "tickSize": "0.25"},
                {"filterType": "LOT_SIZE", "minQty": "0.5", "maxQty": "1000", "stepSize": "0.5"},
                {"filterType": "MIN_NOTIONAL", "minNotional": "10.0", "applyToMarket": true, "avgPriceMins": 5}
            ]},
            {"symbol": "HALTUSDT", "status": "BREAK", "filters": []},
            // zero tick: unusable, must be skipped
            {"symbol": "ZEROUSDT", "status": "TRADING", "filters": [
                {"filterType": "PRICE_FILTER", "minPrice": "0", "maxPrice": "0", "tickSize": "0.00000000"},
                {"filterType": "LOT_SIZE", "minQty": "1", "maxQty": "10", "stepSize": "1"},
                {"filterType": "MIN_NOTIONAL", "minNotional": "5", "applyToMarket": true, "avgPriceMins": 5}
            ]}
        ]})
    }

    // 未核對: the USDⓈ-M filter names (`MIN_NOTIONAL.notional`) are not in the docs copy.
    pub fn futures_info() -> Value {
        json!({"symbols": [
            {"symbol": "BTCUSDT", "status": "TRADING", "contractType": "PERPETUAL", "filters": [
                {"filterType": "PRICE_FILTER", "minPrice": "261.10", "maxPrice": "809484", "tickSize": "0.10"},
                {"filterType": "LOT_SIZE", "minQty": "0.001", "maxQty": "1000", "stepSize": "0.001"},
                {"filterType": "MARKET_LOT_SIZE", "minQty": "0.001", "maxQty": "120", "stepSize": "0.001"},
                {"filterType": "MIN_NOTIONAL", "notional": "100"}
            ]},
            {"symbol": "BTCUSDT_240628", "status": "TRADING", "contractType": "CURRENT_QUARTER", "filters": []}
        ]})
    }

    pub fn okx_swap() -> Value {
        json!({"code": "0", "data": [
            {"instType": "SWAP", "instId": "BTC-USDT-SWAP", "ctVal": "0.01", "ctValCcy": "BTC", "ctType": "linear",
             "lotSz": "1", "minSz": "1", "maxLmtSz": "100000000", "maxMktSz": "20000", "tickSz": "0.1", "state": "live"},
            {"instType": "SWAP", "instId": "BTC-USD-SWAP", "ctVal": "100", "ctValCcy": "USD", "ctType": "inverse",
             "lotSz": "1", "minSz": "1", "maxLmtSz": "100000000", "maxMktSz": "20000", "tickSz": "0.1", "state": "live"},
            // numbers instead of strings: refused
            {"instType": "SWAP", "instId": "ETH-USDT-SWAP", "ctVal": 0.1, "ctType": "linear",
             "lotSz": "1", "minSz": "1", "tickSz": "0.01", "state": "live"}
        ]})
    }

    pub fn okx_spot() -> Value {
        json!({"code": "0", "data": [
            {"instType": "SPOT", "instId": "BTC-USDT", "ctVal": "", "ctType": "",
             "lotSz": "0.00000001", "minSz": "0.00001", "maxLmtSz": "9999999999", "maxMktSz": "1000000",
             "tickSz": "0.1", "state": "live"}
        ]})
    }

    pub fn spot() -> RulesTable {
        RulesTable::from_binance_exchange_info(TradeProduct::BinanceSpot, &spot_info(), 1_000)
            .unwrap()
    }

    pub fn futures() -> RulesTable {
        RulesTable::from_binance_exchange_info(TradeProduct::BinanceUsdM, &futures_info(), 1_000)
            .unwrap()
    }

    pub fn okx_swap_table() -> RulesTable {
        RulesTable::from_okx_instruments(TradeProduct::OkxSwap, &okx_swap(), 1_000).unwrap()
    }

    pub fn okx_spot_table() -> RulesTable {
        RulesTable::from_okx_instruments(TradeProduct::OkxSpot, &okx_spot(), 1_000).unwrap()
    }

    pub fn d(s: &str) -> Decimal {
        Decimal::parse(s).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;
    use serde_json::json;

    #[test]
    fn binance_spot_rules_keep_exact_values() {
        let t = spot();
        let r = t.lookup("BTCUSDT").unwrap();
        assert_eq!(r.tick, d("0.01"));
        assert_eq!(r.lot.step, d("0.00001"));
        assert_eq!(r.lot.min, d("0.00001"));
        assert_eq!(r.lot.max, Some(d("9000")));
        let m = r.market_lot.unwrap();
        assert_eq!(m.max, Some(d("100")));
        let n = r.notional.unwrap();
        assert_eq!(n.min, Some(d("5")));
        assert!(n.min_applies_to_market && !n.max_applies_to_market);
        assert_eq!(r.trailing_delta.unwrap().max_below, 2000);
    }

    #[test]
    fn odd_ticks_are_not_rounded_to_powers_of_ten() {
        let t = spot();
        assert_eq!(t.lookup("ODDUSDT").unwrap().tick, d("0.25"));
    }

    #[test]
    fn unusable_or_halted_symbols_are_excluded() {
        let t = spot();
        assert!(t.lookup("HALTUSDT").is_err());
        assert_eq!(t.lookup("ZEROUSDT").unwrap_err(), TradeError::UnknownSymbol);
        assert_eq!(t.skipped(), ["ZEROUSDT".to_string()]);
    }

    #[test]
    fn futures_rules_and_perpetual_filter() {
        let t = futures();
        let r = t.lookup("BTCUSDT").unwrap();
        assert_eq!(r.tick, d("0.1"));
        assert_eq!(r.notional.unwrap().min, Some(d("100")));
        assert!(t.lookup("BTCUSDT_240628").is_err());
    }

    #[test]
    fn missing_notional_fails_closed() {
        let mut info = futures_info();
        info["symbols"][0]["filters"]
            .as_array_mut()
            .unwrap()
            .retain(|f| f["filterType"] != "MIN_NOTIONAL");
        let t =
            RulesTable::from_binance_exchange_info(TradeProduct::BinanceUsdM, &info, 0).unwrap();
        assert!(t.lookup("BTCUSDT").is_err());
        assert_eq!(t.skipped(), ["BTCUSDT".to_string()]);
    }

    #[test]
    fn okx_rules_parse_contract_facts() {
        let t = okx_swap_table();
        let r = t.lookup("BTC-USDT-SWAP").unwrap();
        assert_eq!(r.tick, d("0.1"));
        assert_eq!(r.lot.step, d("1"));
        assert_eq!(r.lot.min, d("1"));
        let c = r.okx.unwrap();
        assert_eq!(c.ct_val, d("0.01"));
        assert_eq!(c.ct_type, OkxContractType::Linear);
        assert_eq!(
            t.lookup("BTC-USD-SWAP").unwrap().okx.unwrap().ct_type,
            OkxContractType::Inverse
        );
        // JSON numbers are refused
        assert!(t.lookup("ETH-USDT-SWAP").is_err());
        let s = okx_spot_table();
        assert!(s.lookup("BTC-USDT").unwrap().okx.is_none());
    }

    #[test]
    fn symbol_must_exist_and_have_safe_characters() {
        let t = spot();
        assert!(t.validate("BTCUSDT").is_ok());
        assert_eq!(
            t.validate("NOPEUSDT").unwrap_err(),
            TradeError::UnknownSymbol
        );
        for bad in ["BTCUSDT&side=SELL", "BTCUSDT=1", "btcusdt", "", "BTC USDT"] {
            assert!(
                matches!(t.validate(bad), Err(TradeError::Signing(_))),
                "{bad} should be rejected"
            );
        }
    }

    #[test]
    fn rounding_rules() {
        let t = spot();
        let r = t.lookup("BTCUSDT").unwrap();
        // quantity: toward zero
        assert_eq!(r.round_qty(&d("0.123459"), false).unwrap(), d("0.12345"));
        assert_eq!(
            r.round_qty(&d("0.000009"), false),
            Err(TradeError::NonPositive)
        );
        // limit price: buys down, sells up
        assert_eq!(
            r.round_limit_price(Side::Buy, &d("100.019")).unwrap(),
            d("100.01")
        );
        assert_eq!(
            r.round_limit_price(Side::Sell, &d("100.011")).unwrap(),
            d("100.02")
        );
        // trigger: nearest tick
        assert_eq!(r.round_trigger_price(&d("100.014")).unwrap(), d("100.01"));
        assert_eq!(r.round_trigger_price(&d("100.016")).unwrap(), d("100.02"));
        assert_eq!(
            r.round_trigger_price(&d("0.004")),
            Err(TradeError::NonPositive)
        );
        assert!(r.round_qty(&d("-1"), false).is_err());
    }

    #[test]
    fn rounded_output_has_no_exponent() {
        let t = okx_spot_table();
        let r = t.lookup("BTC-USDT").unwrap();
        let q = r.round_qty(&d("0.00000012345"), false).unwrap();
        assert_eq!(q.to_plain_string(), "0.00000012");
    }

    #[test]
    fn freshness_is_checked() {
        let t = spot();
        assert!(t.check_fresh(2_000, 5_000).is_ok());
        assert_eq!(t.check_fresh(10_000, 5_000), Err(TradeError::RulesStale));
        assert_eq!(t.check_fresh(500, 5_000), Err(TradeError::RulesStale));
    }

    #[test]
    fn rules_of_another_product_are_refused() {
        let t = spot();
        let r = t.lookup("BTCUSDT").unwrap();
        assert!(r.require_product(TradeProduct::BinanceSpot).is_ok());
        assert_eq!(
            r.require_product(TradeProduct::BinanceUsdM),
            Err(TradeError::ProductMismatch)
        );
    }

    // ── limit fields: missing vs malformed (F2) ──────────────────────────

    fn okx_spot_with(extra: Value) -> RulesTable {
        let mut item = json!({"instType": "SPOT", "instId": "AAA-USDT", "lotSz": "0.1",
            "minSz": "0.1", "tickSz": "0.01", "state": "live"});
        for (k, v) in extra.as_object().unwrap() {
            item[k] = v.clone();
        }
        RulesTable::from_okx_instruments(
            TradeProduct::OkxSpot,
            &json!({"code": "0", "data": [item]}),
            1,
        )
        .unwrap()
    }

    #[test]
    fn okx_missing_max_fields_mean_no_limit_but_malformed_ones_drop_the_symbol() {
        let t = okx_spot_with(json!({}));
        let r = t.lookup("AAA-USDT").unwrap();
        assert_eq!(r.lot.max, None);
        assert!(r.notional.is_none());

        for bad in [
            json!({"maxMktSz": "abc"}),
            json!({"maxLmtSz": "1e5"}),
            json!({"maxMktSz": 1000}),
            json!({"maxLmtSz": ""}),
        ] {
            let t = okx_spot_with(bad);
            assert!(t.lookup("AAA-USDT").is_err());
            assert_eq!(t.skipped(), ["AAA-USDT".to_string()]);
        }
    }

    // ── Binance zero and missing limits (F3) ─────────────────────────────

    fn binance_with(filters: Value) -> RulesTable {
        let info = json!({"symbols": [{"symbol": "AAAUSDT", "status": "TRADING",
            "filters": filters}]});
        RulesTable::from_binance_exchange_info(TradeProduct::BinanceSpot, &info, 1).unwrap()
    }

    fn price() -> Value {
        json!({"filterType": "PRICE_FILTER", "tickSize": "0.01"})
    }

    fn lot(max: &str) -> Value {
        json!({"filterType": "LOT_SIZE", "minQty": "1", "maxQty": max, "stepSize": "1"})
    }

    fn min_notional() -> Value {
        json!({"filterType": "NOTIONAL", "minNotional": "5"})
    }

    #[test]
    fn zero_max_qty_is_unlimited_only_for_market_lot_size() {
        let market = json!({"filterType": "MARKET_LOT_SIZE", "minQty": "0", "maxQty": "0",
            "stepSize": "1"});
        let t = binance_with(json!([price(), lot("100"), market, min_notional()]));
        let r = t.lookup("AAAUSDT").unwrap();
        assert_eq!(r.market_lot.unwrap().max, None);
        assert_eq!(r.lot.max, Some(d("100")));

        let t = binance_with(json!([price(), lot("0"), min_notional()]));
        assert!(t.lookup("AAAUSDT").is_err());
        assert_eq!(t.skipped(), ["AAAUSDT".to_string()]);
    }

    #[test]
    fn notional_filter_without_min_notional_fails_closed() {
        let no_min = json!({"filterType": "NOTIONAL", "maxNotional": "1000"});
        let t = binance_with(json!([price(), lot("100"), no_min]));
        assert!(t.lookup("AAAUSDT").is_err());
        assert!(
            binance_with(json!([price(), lot("100"), min_notional()]))
                .lookup("AAAUSDT")
                .is_ok()
        );
    }

    // ── 1e-9 ticks and steps (F7) ────────────────────────────────────────

    #[test]
    fn rounding_with_nano_tick_and_step() {
        let t = binance_with(json!([
            json!({"filterType": "PRICE_FILTER", "tickSize": "0.000000001"}),
            json!({"filterType": "LOT_SIZE", "minQty": "0.000000001", "maxQty": "1000",
                "stepSize": "0.000000001"}),
            min_notional()
        ]));
        let r = t.lookup("AAAUSDT").unwrap();
        // quantity towards zero
        assert_eq!(
            r.round_qty(&d("0.0000000019"), false).unwrap(),
            d("0.000000001")
        );
        assert_eq!(
            r.round_qty(&d("1.9999999999"), false).unwrap(),
            d("1.999999999")
        );
        assert_eq!(
            r.round_qty(&d("0.0000000009"), false).unwrap_err(),
            TradeError::NonPositive
        );
        // limit price: buys down, sells up, exact multiples unchanged
        assert_eq!(
            r.round_limit_price(Side::Buy, &d("1.0000000019")).unwrap(),
            d("1.000000001")
        );
        assert_eq!(
            r.round_limit_price(Side::Sell, &d("1.0000000011")).unwrap(),
            d("1.000000002")
        );
        assert_eq!(
            r.round_limit_price(Side::Sell, &d("1.000000001")).unwrap(),
            d("1.000000001")
        );
        // trigger: nearest tick, a tie rounds up
        assert_eq!(
            r.round_trigger_price(&d("1.0000000014")).unwrap(),
            d("1.000000001")
        );
        assert_eq!(
            r.round_trigger_price(&d("1.0000000015")).unwrap(),
            d("1.000000002")
        );
    }
}
