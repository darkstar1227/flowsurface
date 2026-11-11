//! What the entry confirmation pre-authorises (M5 d3).
//!
//! An [`AutoGrant`] is made from the confirmed entry request itself (account, symbol, direction,
//! quantity and client id are read from the `OrderIntent`, never passed in as strings) together
//! with the list of protective roles the confirmation window showed. The automatic path then
//! sends only what the grant covers:
//!
//! - a protective order whose client id the grant issued, for one of the listed roles, on the
//!   grant's account and symbol, on the closing side, reducing, and for no more than the
//!   quantity filled so far, before the grant expires;
//! - the cancel of an order the grant itself sent (cancelling stays possible after the grant
//!   expired: the two protective orders must keep cancelling each other).
//!
//! The grant is also bound to the entry it was made for and to the protection settings the
//! confirmation showed: a placement must name the same entry client id and carry the fingerprint
//! of the same [`ProtectSpec`], and the `reduceOnly` quantity authorised so far for a role never
//! exceeds what has filled (the take-profit legs together stay within the fill). Fills are
//! accepted only for that entry.
//!
//! Every id is single use and a role is issued once: a failed or refused send is never tried
//! again under the grant. The grant is not `Clone`; [`GrantedAuto`] (what `submit_auto` takes)
//! can only be made by [`AutoGrant::authorise`].
//!
//! ```compile_fail,E0599
//! use flowsurface_exchange::trade::AutoGrant;
//! fn twice(g: &AutoGrant) -> AutoGrant { g.clone() }
//! ```
//!
//! ```compile_fail,E0451
//! use flowsurface_exchange::trade::{AutoIntent, GrantedAuto};
//! // Only `AutoGrant::authorise` makes one.
//! fn forge(auto: AutoIntent) -> GrantedAuto { GrantedAuto { auto } }
//! ```

use super::binance_futures;
use super::client_id::{ClientId, Role};
use super::decimal::Decimal;
use super::gate::{AutoIntent, AutoPath, OrderIntent, Prepared};
use super::protect::{ProtectSpec, SpecFingerprint};
use super::request::{BinanceRequest, Exposure, Method};
use super::rules::FreshRules;
use super::types::{Dir, OrderRef, TradeError};
use crate::account::AccountKey;
use std::cmp::Ordering;
use std::fmt;

const ALGO_PATH: &str = "/fapi/v1/algoOrder";
const ORDER_PATH: &str = "/fapi/v1/order";
/// Most protective orders one grant can issue (one stop, three targets, one trailing stop).
const MAX_ISSUED: usize = 5;

/// The protective roles the confirmation listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GrantRoles {
    pub stop: bool,
    /// Take-profit orders (at most three).
    pub targets: u8,
    pub trailing: bool,
}

impl GrantRoles {
    pub fn is_empty(&self) -> bool {
        !self.stop && self.targets == 0 && !self.trailing
    }
}

/// Why the grant did not cover a request. Nothing was sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantRefusal {
    /// Not a Binance USDⓈ-M entry, or a request the automatic path does not know.
    Unsupported,
    WrongAccount,
    WrongSymbol,
    /// The path and the request disagree (a cancel under the attach path, or the reverse).
    PathMismatch,
    /// The request does not reduce.
    NotReducing,
    /// The role is not in the confirmed list.
    RoleNotListed,
    /// The role was issued already (a send is never repeated).
    RoleSpent,
    /// The client id is not one this grant issued.
    UnknownId,
    /// The id was used (or never sent, for a cancel).
    IdNotAvailable,
    AlreadyCancelled,
    Expired,
    /// Nothing is filled yet.
    NoFill,
    QtyTooLarge,
    WrongSide,
    WrongPositionSide,
    /// A placement that does not say which entry and settings it is based on.
    NotBound,
    /// The placement is based on another entry than the grant's.
    WrongEntry,
    /// The placement is based on other protection settings than the confirmed ones.
    SpecMismatch,
    /// The `reduceOnly` quantity authorised for the role would pass what has filled.
    TotalTooLarge,
}

impl fmt::Display for GrantRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            GrantRefusal::Unsupported => "the automatic path does not cover this request",
            GrantRefusal::WrongAccount => "the grant belongs to another account",
            GrantRefusal::WrongSymbol => "the grant belongs to another symbol",
            GrantRefusal::PathMismatch => "the request does not fit the automatic path",
            GrantRefusal::NotReducing => "the request does not only reduce",
            GrantRefusal::RoleNotListed => "this order was not in the confirmed list",
            GrantRefusal::RoleSpent => "this order was already sent once",
            GrantRefusal::UnknownId => "the order was not issued by the grant",
            GrantRefusal::IdNotAvailable => "the order id cannot be used",
            GrantRefusal::AlreadyCancelled => "the order was already cancelled",
            GrantRefusal::Expired => "the grant expired",
            GrantRefusal::NoFill => "nothing is filled yet",
            GrantRefusal::QtyTooLarge => "the quantity is more than what filled",
            GrantRefusal::WrongSide => "the order is not on the closing side",
            GrantRefusal::WrongPositionSide => "the position side does not match",
            GrantRefusal::NotBound => "the order does not say which entry it protects",
            GrantRefusal::WrongEntry => "the order is for another entry",
            GrantRefusal::SpecMismatch => "the order does not match the confirmed settings",
            GrantRefusal::TotalTooLarge => "the quantities together are more than what filled",
        })
    }
}

/// Which issued order: an index, so a caller never handles the id text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct IssuedRef(u8);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Issued, not yet part of a request that was authorised.
    Issued,
    /// Authorised for sending.
    Sent,
    /// Its cancel was authorised.
    Cancelled,
}

struct Issued {
    id: String,
    role: Role,
    state: State,
}

/// What a placement is based on: the entry's client id and the protection settings. Never
/// printed (it holds a client id).
#[derive(Clone)]
pub struct Basis {
    entry_id: String,
    spec: SpecFingerprint,
}

impl Basis {
    pub fn new(entry_id: &str, spec: &ProtectSpec) -> Self {
        Basis {
            entry_id: entry_id.to_string(),
            spec: spec.fingerprint(),
        }
    }
}

impl fmt::Debug for Basis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Basis(<redacted>)")
    }
}

pub struct AutoGrant {
    account: AccountKey,
    /// The client id of the entry this grant was made for.
    entry_id: String,
    spec: SpecFingerprint,
    /// `reduceOnly` quantity authorised so far, per role.
    reduce_authorised: Vec<(Role, Decimal)>,
    symbol: String,
    dir: Dir,
    entry_qty: Decimal,
    roles: GrantRoles,
    expires_ms: i64,
    filled: Decimal,
    issued: Vec<Issued>,
}

impl fmt::Debug for AutoGrant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AutoGrant(<redacted>)")
    }
}

/// An automatic request the grant covers. The only thing `submit_auto` takes.
#[derive(Debug)]
pub struct GrantedAuto {
    pub(crate) auto: AutoIntent,
}

impl GrantedAuto {
    pub fn path(&self) -> AutoPath {
        self.auto.path()
    }
}

/// What a successful check says to record.
struct Checked {
    index: usize,
    state: State,
    /// The new total of `reduceOnly` quantity authorised for the role.
    reduce_total: Option<(Role, Decimal)>,
}

fn binance(intent: &OrderIntent) -> Result<&BinanceRequest, GrantRefusal> {
    match &intent.prepared {
        Prepared::Binance(r) => Ok(r),
        Prepared::Okx(_) => Err(GrantRefusal::Unsupported),
    }
}

fn dec(r: &BinanceRequest, name: &str) -> Option<Decimal> {
    r.param(name).and_then(|v| Decimal::parse(v).ok())
}

impl AutoGrant {
    /// Made when the user confirms an entry, from the entry request itself. Only a Binance
    /// USDⓈ-M entry that adds exposure has a grant (OKX carries its protection on the entry).
    pub fn from_entry(
        intent: &OrderIntent,
        roles: GrantRoles,
        spec: &ProtectSpec,
        valid_for_ms: i64,
        now_ms: i64,
    ) -> Result<Self, GrantRefusal> {
        let r = binance(intent)?;
        let entry_id = match r.client_ids().as_slice() {
            [one] => (*one).to_string(),
            _ => return Err(GrantRefusal::Unsupported),
        };
        if r.method() != Method::Post
            || r.path() != ORDER_PATH
            || !matches!(intent.exposure(), Exposure::Adds { .. })
            || roles.is_empty()
            || roles.targets > 3
        {
            return Err(GrantRefusal::Unsupported);
        }
        let dir = match r.param("side") {
            Some("BUY") => Dir::Long,
            Some("SELL") => Dir::Short,
            _ => return Err(GrantRefusal::Unsupported),
        };
        let entry_qty = dec(r, "quantity")
            .filter(Decimal::is_positive)
            .ok_or(GrantRefusal::Unsupported)?;
        Ok(AutoGrant {
            account: intent.account(),
            entry_id,
            spec: spec.fingerprint(),
            reduce_authorised: Vec::new(),
            symbol: intent.symbol().to_string(),
            dir,
            entry_qty,
            roles,
            expires_ms: now_ms.saturating_add(valid_for_ms),
            filled: Decimal::ZERO,
            issued: Vec::new(),
        })
    }

    pub fn dir(&self) -> Dir {
        self.dir
    }

    pub fn roles(&self) -> GrantRoles {
        self.roles
    }

    /// The quantity filled so far, never more than the entry's quantity.
    pub fn filled(&self) -> Decimal {
        self.filled
    }

    /// Fills are added by the app as they arrive; the total is capped at the entry quantity.
    /// Only a fill of this grant's own entry counts: `false` (and nothing added) otherwise.
    pub fn add_fill(&mut self, entry_id: &str, qty: Decimal) -> bool {
        if entry_id != self.entry_id {
            return false;
        }
        if !qty.is_positive() {
            return true;
        }
        let total = self.filled.checked_add(&qty).unwrap_or(self.entry_qty);
        self.filled = match total.checked_cmp(&self.entry_qty) {
            Ok(Ordering::Greater) | Err(_) => self.entry_qty,
            Ok(_) => total,
        };
        true
    }

    fn listed(&self, role: Role) -> bool {
        let count = self.issued.iter().filter(|i| i.role == role).count();
        match role {
            Role::StopLoss => self.roles.stop && count < 1,
            Role::TakeProfit => count < usize::from(self.roles.targets),
            Role::Trailing => self.roles.trailing && count < 1,
            Role::Entry => false,
        }
    }

    /// Issue the client id of one protective order. A role is issued once per list entry; asking
    /// again after a failure is refused, so nothing is retried under this grant.
    pub fn issue(
        &mut self,
        role: Role,
        now_ms: i64,
    ) -> Result<(IssuedRef, ClientId), GrantRefusal> {
        if now_ms > self.expires_ms {
            return Err(GrantRefusal::Expired);
        }
        if !self.listed(role) {
            let listed_at_all = match role {
                Role::StopLoss => self.roles.stop,
                Role::TakeProfit => self.roles.targets > 0,
                Role::Trailing => self.roles.trailing,
                Role::Entry => false,
            };
            return Err(if listed_at_all {
                GrantRefusal::RoleSpent
            } else {
                GrantRefusal::RoleNotListed
            });
        }
        if self.issued.len() >= MAX_ISSUED {
            return Err(GrantRefusal::RoleSpent);
        }
        let id = ClientId::generate(role);
        let index = u8::try_from(self.issued.len()).map_err(|_| GrantRefusal::RoleSpent)?;
        self.issued.push(Issued {
            id: id.as_str().to_string(),
            role,
            state: State::Issued,
        });
        Ok((IssuedRef(index), id))
    }

    fn index_of(&self, id: &str) -> Option<usize> {
        self.issued.iter().position(|i| i.id == id)
    }

    /// A cancel request for an order this grant sent. It is checked again, and the order marked
    /// cancelled, by [`AutoGrant::authorise`].
    pub fn cancel_intent(
        &self,
        which: IssuedRef,
        fresh: &FreshRules<'_>,
    ) -> Result<AutoIntent, GrantRefusal> {
        let issued = self
            .issued
            .get(usize::from(which.0))
            .ok_or(GrantRefusal::UnknownId)?;
        match issued.state {
            State::Sent => {}
            State::Cancelled => return Err(GrantRefusal::AlreadyCancelled),
            State::Issued => return Err(GrantRefusal::IdNotAvailable),
        }
        let id = ClientId::parse(&issued.id).map_err(|_| GrantRefusal::UnknownId)?;
        let request = binance_futures::cancel_algo_order(&OrderRef::Client(id))
            .map_err(|_| GrantRefusal::Unsupported)?;
        let intent = OrderIntent::binance(self.account, request, fresh)
            .map_err(|_: TradeError| GrantRefusal::Unsupported)?;
        AutoIntent::new(AutoPath::CancelSibling, intent).map_err(|_| GrantRefusal::NotReducing)
    }

    /// Check `auto` against the grant. On success the request is marked as authorised (a placed
    /// id becomes spent, a cancelled id is cancelled), so asking again is refused.
    pub fn authorise(
        &mut self,
        auto: AutoIntent,
        now_ms: i64,
    ) -> Result<GrantedAuto, GrantRefusal> {
        let checked = self.check(&auto, now_ms)?;
        self.issued[checked.index].state = checked.state;
        if let Some((role, total)) = checked.reduce_total {
            // Counted when authorised, never given back: a failed send is not retried.
            match self.reduce_authorised.iter_mut().find(|(r, _)| *r == role) {
                Some(slot) => slot.1 = total,
                None => self.reduce_authorised.push((role, total)),
            }
        }
        Ok(GrantedAuto { auto })
    }

    fn check(&self, auto: &AutoIntent, now_ms: i64) -> Result<Checked, GrantRefusal> {
        let intent = &auto.intent;
        if intent.account() != self.account {
            return Err(GrantRefusal::WrongAccount);
        }
        if intent.symbol() != self.symbol {
            return Err(GrantRefusal::WrongSymbol);
        }
        let r = binance(intent)?;
        if r.path() != ALGO_PATH {
            return Err(GrantRefusal::Unsupported);
        }
        let ids = r.client_ids();
        let [id] = ids.as_slice() else {
            return Err(GrantRefusal::Unsupported);
        };
        let index = self.index_of(id).ok_or(GrantRefusal::UnknownId)?;
        let issued = &self.issued[index];
        match (auto.path(), r.method()) {
            (AutoPath::AttachProtective, Method::Post) => {
                let reduce_total =
                    self.check_place(r, issued, intent, auto.basis.as_ref(), now_ms)?;
                Ok(Checked {
                    index,
                    state: State::Sent,
                    reduce_total,
                })
            }
            (AutoPath::CancelSibling, Method::Delete) => {
                if !matches!(intent.exposure(), Exposure::Neutral) {
                    return Err(GrantRefusal::NotReducing);
                }
                match issued.state {
                    State::Sent => Ok(Checked {
                        index,
                        state: State::Cancelled,
                        reduce_total: None,
                    }),
                    State::Cancelled => Err(GrantRefusal::AlreadyCancelled),
                    State::Issued => Err(GrantRefusal::IdNotAvailable),
                }
            }
            _ => Err(GrantRefusal::PathMismatch),
        }
    }

    fn check_place(
        &self,
        r: &BinanceRequest,
        issued: &Issued,
        intent: &OrderIntent,
        basis: Option<&Basis>,
        now_ms: i64,
    ) -> Result<Option<(Role, Decimal)>, GrantRefusal> {
        if !matches!(intent.exposure(), Exposure::Reduces) {
            return Err(GrantRefusal::NotReducing);
        }
        if issued.state != State::Issued {
            return Err(GrantRefusal::IdNotAvailable);
        }
        if now_ms > self.expires_ms {
            return Err(GrantRefusal::Expired);
        }
        let role = match r.param("type") {
            Some("STOP_MARKET" | "STOP") => Role::StopLoss,
            Some("TAKE_PROFIT_MARKET" | "TAKE_PROFIT") => Role::TakeProfit,
            Some("TRAILING_STOP_MARKET") => Role::Trailing,
            _ => return Err(GrantRefusal::Unsupported),
        };
        if role != issued.role {
            return Err(GrantRefusal::RoleNotListed);
        }
        if r.param("side") != Some(self.dir.closing_side().binance()) {
            return Err(GrantRefusal::WrongSide);
        }
        let position_side = match self.dir {
            Dir::Long => "LONG",
            Dir::Short => "SHORT",
        };
        if r.param("positionSide").is_some_and(|p| p != position_side) {
            return Err(GrantRefusal::WrongPositionSide);
        }
        if !self.filled.is_positive() {
            return Err(GrantRefusal::NoFill);
        }
        let basis = basis.ok_or(GrantRefusal::NotBound)?;
        if basis.entry_id != self.entry_id {
            return Err(GrantRefusal::WrongEntry);
        }
        if basis.spec != self.spec {
            return Err(GrantRefusal::SpecMismatch);
        }
        match (r.param("closePosition"), r.param("reduceOnly")) {
            // Closes the whole position: no quantity, nothing to compare.
            (Some("true"), _) => Ok(None),
            (_, Some("true")) => {
                let qty = dec(r, "quantity")
                    .filter(Decimal::is_positive)
                    .ok_or(GrantRefusal::Unsupported)?;
                if !matches!(
                    qty.checked_cmp(&self.filled),
                    Ok(Ordering::Less | Ordering::Equal)
                ) {
                    return Err(GrantRefusal::QtyTooLarge);
                }
                // The legs of a role together stay within the fill.
                let so_far = self
                    .reduce_authorised
                    .iter()
                    .find(|(r, _)| *r == role)
                    .map_or(Decimal::ZERO, |(_, t)| *t);
                let total = so_far
                    .checked_add(&qty)
                    .map_err(|_| GrantRefusal::TotalTooLarge)?;
                if !matches!(
                    total.checked_cmp(&self.filled),
                    Ok(Ordering::Less | Ordering::Equal)
                ) {
                    return Err(GrantRefusal::TotalTooLarge);
                }
                Ok(Some((role, total)))
            }
            // Hedge mode forbids reduceOnly; a hedge order must close the position.
            _ => Err(GrantRefusal::NotReducing),
        }
    }
}

#[cfg(test)]
mod tests;
