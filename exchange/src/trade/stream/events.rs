//! The one event type every private stream produces (for the order panel, protective orders
//! and the risk controls), and the helpers the venue parsers share.
//!
//! Private events are untrusted input (condition 52): parsers never `unwrap`, never panic,
//! read numbers only from decimal strings (no `f32`/`f64`), and on a bad frame report a
//! coarse [`ParseError`] that carries no frame content.

use crate::trade::decimal::Decimal;
use crate::trade::types::Side;
use serde_json::Value;

/// The largest private frame accepted. Real frames are a few kilobytes.
pub const MAX_FRAME_BYTES: usize = 1 << 20;

/// Why a frame could not be read. Never carries the frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    TooLarge,
    NotJson,
    /// A field the event needs is missing or has the wrong type or format.
    Shape,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ParseError::TooLarge => "frame too large",
            ParseError::NotJson => "frame is not JSON",
            ParseError::Shape => "unexpected frame shape",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderCategory {
    Plain,
    /// Binance `ALGO_UPDATE`, OKX `orders-algo` / `algo-advance`.
    Algo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderStatus {
    New,
    PartiallyFilled,
    Filled,
    Canceled,
    Expired,
    Rejected,
    /// An algo order fired (its child order is placed).
    Triggered,
    /// An algo order is done.
    Finished,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionSide {
    /// One-way mode (Binance `BOTH`, OKX `net`).
    Both,
    Long,
    Short,
}

#[derive(Clone, PartialEq, Eq)]
pub struct OrderUpdate {
    pub symbol: String,
    pub client_id: Option<String>,
    pub order_id: Option<String>,
    pub category: OrderCategory,
    pub side: Option<Side>,
    pub status: OrderStatus,
    pub price: Option<Decimal>,
    pub qty: Option<Decimal>,
    pub filled_qty: Option<Decimal>,
    pub reduce_only: Option<bool>,
    pub position_side: Option<PositionSide>,
    pub time_ms: Option<i64>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct Fill {
    pub symbol: String,
    pub client_id: Option<String>,
    pub order_id: Option<String>,
    pub trade_id: Option<String>,
    pub side: Option<Side>,
    pub qty: Decimal,
    pub price: Decimal,
    /// As the exchange reports it (Binance: a cost as a positive number; OKX: a cost as a
    /// negative number).
    pub fee: Option<Decimal>,
    pub fee_asset: Option<String>,
    pub realized_pnl: Option<Decimal>,
    pub time_ms: Option<i64>,
}

/// The client id ties an order to this app and to an Unknown lock: it is not written to logs, so
/// `Debug` shows only whether there is one. (`PrivateEvent` derives `Debug` from these.)
fn redacted(id: &Option<String>) -> &'static str {
    if id.is_some() { "<redacted>" } else { "None" }
}

impl std::fmt::Debug for OrderUpdate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OrderUpdate")
            .field("symbol", &self.symbol)
            .field("client_id", &format_args!("{}", redacted(&self.client_id)))
            .field("order_id", &self.order_id)
            .field("category", &self.category)
            .field("side", &self.side)
            .field("status", &self.status)
            .field("price", &self.price)
            .field("qty", &self.qty)
            .field("filled_qty", &self.filled_qty)
            .field("reduce_only", &self.reduce_only)
            .field("position_side", &self.position_side)
            .field("time_ms", &self.time_ms)
            .finish()
    }
}

impl std::fmt::Debug for Fill {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fill")
            .field("symbol", &self.symbol)
            .field("client_id", &format_args!("{}", redacted(&self.client_id)))
            .field("order_id", &self.order_id)
            .field("trade_id", &self.trade_id)
            .field("side", &self.side)
            .field("qty", &self.qty)
            .field("price", &self.price)
            .field("fee", &self.fee)
            .field("fee_asset", &self.fee_asset)
            .field("realized_pnl", &self.realized_pnl)
            .field("time_ms", &self.time_ms)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PositionUpdate {
    pub symbol: String,
    pub position_side: PositionSide,
    /// The exchange's size as reported: signed for one-way mode, zero when flat.
    pub qty: Decimal,
    pub entry_price: Option<Decimal>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BalanceUpdate {
    pub asset: String,
    /// Available to trade, when the event says so.
    pub free: Option<Decimal>,
    /// Wallet balance or equity of the asset, when the event says so.
    pub total: Option<Decimal>,
}

/// Total account equity as the exchange defines it (OKX `totalEq`). Binance's
/// `totalMarginBalance` is not pushed; segment e reads it by REST.
///
/// `non_exhaustive`: only the parsers of this crate can build one, so a reading cannot be
/// passed off as coming from the private stream (review item 8).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct EquityUpdate {
    pub(crate) total_equity: Decimal,
    /// Whether the venue's stream tells a transfer apart from a trading result. Set by the
    /// parser that made the update, never by a caller (review B2): OKX's equity push does not.
    pub(crate) transfer_flags: bool,
}

impl EquityUpdate {
    pub(crate) fn new(total_equity: Decimal, transfer_flags: bool) -> Self {
        Self {
            total_equity,
            transfer_flags,
        }
    }

    pub fn total_equity(&self) -> Decimal {
        self.total_equity
    }
}

/// Why an account changed (Binance `ACCOUNT_UPDATE` `m`). Segment e uses it to re-base the
/// equity high-water mark after a transfer (condition 73).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountReason {
    Order,
    FundingFee,
    Deposit,
    Withdraw,
    /// Moves between the user's own wallets or accounts.
    Transfer,
    Other,
}

impl AccountReason {
    /// Money moved in or out without trading: equity jumps that are not profit or loss.
    pub fn is_transfer(self) -> bool {
        matches!(
            self,
            AccountReason::Deposit | AccountReason::Withdraw | AccountReason::Transfer
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResyncReason {
    /// An order status this build does not know.
    UnknownOrderStatus,
    /// An order, position or balance frame that could not be read.
    MalformedFrame,
    /// Spot order-list status changed (read by REST).
    ListStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrivateEvent {
    Order(OrderUpdate),
    Fill(Fill),
    Position(PositionUpdate),
    Balance(BalanceUpdate),
    Equity(EquityUpdate),
    Account(AccountReason),
    /// Binance told us the listenKey is no longer valid.
    ListenKeyExpired,
    /// The exchange is ending this stream (Binance spot `eventStreamTerminated`, OKX notice).
    StreamTerminated,
    /// Local state may be wrong: read it again over REST before trusting it.
    ResyncRequired(ResyncReason),
}

// ── Shared parsing helpers ──────────────────────────────────────────────

pub(crate) fn decode(frame: &[u8]) -> Result<Value, ParseError> {
    if frame.len() > MAX_FRAME_BYTES {
        return Err(ParseError::TooLarge);
    }
    serde_json::from_slice(frame).map_err(|_| ParseError::NotJson)
}

pub(crate) fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// A decimal from a string field. Absent or empty is `Ok(None)`; a value that is present but
/// not a decimal string (a JSON number, an object, `1e5`, garbage) is an error, so a number
/// never goes through a float.
pub(crate) fn decimal_opt(v: &Value, key: &str) -> Result<Option<Decimal>, ParseError> {
    match v.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.is_empty() => Ok(None),
        Some(Value::String(s)) => Decimal::parse(s).map(Some).map_err(|_| ParseError::Shape),
        Some(_) => Err(ParseError::Shape),
    }
}

pub(crate) fn decimal_req(v: &Value, key: &str) -> Result<Decimal, ParseError> {
    decimal_opt(v, key)?.ok_or(ParseError::Shape)
}

pub(crate) fn symbol_req(v: &Value, key: &str) -> Result<String, ParseError> {
    let s = str_of(v, key).ok_or(ParseError::Shape)?;
    crate::signing::validate_symbol(s).map_err(|_| ParseError::Shape)?;
    Ok(s.to_string())
}

const MAX_ID_LEN: usize = 64;

/// An order or trade id: a string or an integer, short, no whitespace or control characters.
pub(crate) fn id_opt(v: &Value, key: &str) -> Option<String> {
    let raw = match v.get(key)? {
        Value::String(s) => s.clone(),
        Value::Number(n) if n.is_i64() || n.is_u64() => n.to_string(),
        _ => return None,
    };
    let ok = !raw.is_empty()
        && raw.len() <= MAX_ID_LEN
        && raw
            .bytes()
            .all(|b| b.is_ascii_graphic() && !matches!(b, b'"' | b'\\'));
    ok.then_some(raw)
}

pub(crate) fn int_opt(v: &Value, key: &str) -> Option<i64> {
    match v.get(key)? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

pub(crate) fn side_of(raw: Option<&str>) -> Option<Side> {
    match raw? {
        "BUY" | "buy" => Some(Side::Buy),
        "SELL" | "sell" => Some(Side::Sell),
        _ => None,
    }
}

/// An event or channel name that is safe to write to a log: short and made of letters,
/// digits, `_`, `-` only. Anything else is replaced, so a hostile frame cannot inject text.
pub(crate) fn loggable_name(raw: &str) -> &str {
    let ok = !raw.is_empty()
        && raw.len() <= 40
        && raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if ok { raw } else { "?" }
}

/// An exchange error code that is safe to show or log: `[-0-9A-Za-z]`, at most 16 characters.
pub(crate) fn clean_code(raw: &str) -> String {
    let ok = !raw.is_empty()
        && raw.len() <= 16
        && raw.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    if ok { raw.to_string() } else { "?".to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "eAbCdEfGhIjKlMnOpQrStUvW";

    fn order() -> OrderUpdate {
        OrderUpdate {
            symbol: "BTCUSDT".into(),
            client_id: Some(ID.into()),
            order_id: Some("42".into()),
            category: OrderCategory::Plain,
            side: None,
            status: OrderStatus::New,
            price: None,
            qty: None,
            filled_qty: None,
            reduce_only: None,
            position_side: None,
            time_ms: None,
        }
    }

    fn fill() -> Fill {
        Fill {
            symbol: "BTCUSDT".into(),
            client_id: Some(ID.into()),
            order_id: None,
            trade_id: Some("7".into()),
            side: None,
            qty: Decimal::parse("1").unwrap(),
            price: Decimal::parse("2").unwrap(),
            fee: None,
            fee_asset: None,
            realized_pnl: None,
            time_ms: None,
        }
    }

    #[test]
    fn debug_output_hides_the_client_id_everywhere_an_event_can_be_printed() {
        let texts = [
            format!("{:?}", order()),
            format!("{:#?}", order()),
            format!("{:?}", fill()),
            format!("{:?}", PrivateEvent::Order(order())),
            format!("{:?}", PrivateEvent::Fill(fill())),
            format!("{:#?}", Some(PrivateEvent::Fill(fill()))),
        ];
        for t in texts {
            assert!(!t.contains(ID), "{t}");
            assert!(t.contains("<redacted>"), "{t}");
        }
        // The rest is still there for diagnosis.
        assert!(format!("{:?}", order()).contains("BTCUSDT"));
        // No id at all reads as `None`, not as redacted.
        let mut none = order();
        none.client_id = None;
        assert!(format!("{none:?}").contains("client_id: None"));
    }
}
