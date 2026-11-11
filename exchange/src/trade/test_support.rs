//! Helpers for the tests of crates that depend on this one (feature `test-support`, enabled
//! by them as a dev-dependency only). Nothing here is part of a normal build.

use super::rules::{FreshRules, RulesTable, TradeProduct};
use serde_json::json;

/// Rules for BTCUSDT on Binance USDⓈ-M (tick 0.1, step 0.001), read through the same parser as
/// the live rules.
pub fn futures_table() -> RulesTable {
    let info = json!({"symbols": [
        {"symbol": "BTCUSDT", "status": "TRADING", "contractType": "PERPETUAL", "filters": [
            {"filterType": "PRICE_FILTER", "minPrice": "261.10", "maxPrice": "809484", "tickSize": "0.10"},
            {"filterType": "LOT_SIZE", "minQty": "0.001", "maxQty": "1000", "stepSize": "0.001"},
            {"filterType": "MARKET_LOT_SIZE", "minQty": "0.001", "maxQty": "120", "stepSize": "0.001"},
            {"filterType": "MIN_NOTIONAL", "notional": "100"}
        ]}
    ]});
    RulesTable::from_binance_exchange_info(TradeProduct::BinanceUsdM, &info, 1_000)
        .expect("fixed test rules parse")
}

/// The rules of `symbol` as fresh at `now_ms`.
pub fn fresh<'a>(table: &'a RulesTable, symbol: &str, now_ms: i64) -> FreshRules<'a> {
    table
        .fresh_lookup(symbol, now_ms, 60_000)
        .expect("test rules are fresh")
}
