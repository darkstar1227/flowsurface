//! The connection state machine of one account's private stream (conditions 47, 49, 50, 51).
//!
//! It is a pure value: it takes what happened (a connection came up or went down, an event
//! arrived, time passed) and answers with commands for the driver. It reads no clock (the
//! caller passes a monotonic `now`), opens no socket and sends no request, so every rule here
//! is tested with a fake clock.

use crate::adapter::ws::ReconnectBackoff;
use std::time::Duration;

/// How often a futures listenKey is extended. The key lives 60 minutes and the rule is at
/// most 30 (condition 49).
pub const KEEPALIVE_EVERY: Duration = Duration::from_secs(25 * 60);

/// The least time between the starts of two REST reads. Events that ask for a read (a frame
/// that could not be read, a status this build does not know) arrive as fast as the exchange
/// sends them; the reads are signed and use the shared rate limit, so they are spaced out and
/// the requests that arrive meanwhile are merged into one.
pub const MIN_RESYNC_INTERVAL: Duration = Duration::from_secs(2);

/// Where a private stream stands. `Connected` means connected **and** synchronised by REST;
/// connected but not yet synchronised is `Resyncing`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StreamState {
    Connected,
    Reconnecting,
    Resyncing,
    /// The exchange refused the login or key. No automatic reconnect.
    AuthFailed,
    /// No private stream exists for this account (spot until verified, condition 48).
    Unavailable,
}

/// What the user or the app wants to do with orders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TradeAction {
    /// A new entry order.
    NewEntry,
    /// Dragging an order to a new price (cancel and place again).
    DragModify,
    Cancel,
    Close,
    CancelAll,
    CloseAll,
}

/// Condition 51: unless the stream is `Connected` and synchronised, new entries and drags are
/// refused (the order and position lists may be stale); cancelling and closing stay possible
/// so a position can always be cut.
pub fn allows(state: StreamState, action: TradeAction) -> bool {
    match action {
        TradeAction::NewEntry | TradeAction::DragModify => state == StreamState::Connected,
        TradeAction::Cancel
        | TradeAction::Close
        | TradeAction::CancelAll
        | TradeAction::CloseAll => true,
    }
}

/// The order and position lists may be out of date and are to be shown as such.
pub fn lists_may_be_stale(state: StreamState) -> bool {
    state != StreamState::Connected
}

/// "Cancel the other leg when one fills" waits until the stream is synchronised again, then
/// is done after the REST read.
pub fn one_cancels_other_paused(state: StreamState) -> bool {
    state != StreamState::Connected
}

/// Why one connection ended or could not start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownCause {
    /// Network error, timeout, close frame: reconnect with a backoff.
    Network,
    /// The exchange refused the key or signature: stop, do not reconnect.
    Auth,
    /// There is no stream to connect (spot, unverified).
    Unavailable,
    /// The clock is off (a login or subscribe was refused for its timestamp): re-measure the
    /// clock, then reconnect with a backoff.
    ClockSkew,
    /// The Keychain item could not be loaded: the session verdict is void, retry with a
    /// backoff.
    Keychain,
    /// The proxy setting changed or is broken: do not connect (condition 42), retry later in
    /// case the setting is restored.
    Proxy,
    /// listenKey expired or could not be extended, or the exchange ended the stream.
    Expired,
}

/// Why a trade session verdict must be voided (condition 57).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidReason {
    KeychainUnavailable,
    StreamAuthFailed,
}

/// What the driver should do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// (Re)start connection `leg` after `after`; replaces any connection it has.
    Connect { leg: usize, after: Duration },
    /// Read orders, positions and balances over REST.
    Resync { after: Duration },
    /// Extend the listenKey.
    KeepAlive,
    /// Re-measure the clock offset.
    SyncClock,
    /// Void the account's trade session verdict.
    InvalidateSession(InvalidReason),
    /// Close every connection and do nothing more (after an authentication failure or when
    /// the stream is unavailable).
    Stop,
    /// Close the listenKey at the exchange and drop it (account deleted or disabled).
    DeleteKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Leg {
    Down,
    Up,
}

pub struct Machine {
    legs: Vec<Leg>,
    backoffs: Vec<ReconnectBackoff>,
    resync_backoff: ReconnectBackoff,
    uses_listen_key: bool,
    synced: bool,
    resync_running: bool,
    resync_again: bool,
    /// The newest `now` the machine has been told (it reads no clock itself).
    now: Duration,
    /// When the latest REST read was (or will be) started.
    last_resync_start: Option<Duration>,
    auth_failed: bool,
    unavailable: bool,
    session_voided: bool,
    keepalive_due: Option<Duration>,
    keepalive_running: bool,
}

impl Machine {
    /// `legs` connections (Binance: 1, OKX: 2). `uses_listen_key`: Binance USDⓈ-M.
    pub fn start(legs: usize, uses_listen_key: bool) -> (Self, Vec<Command>) {
        let m = Machine {
            legs: vec![Leg::Down; legs],
            backoffs: (0..legs).map(|_| ReconnectBackoff::new()).collect(),
            resync_backoff: ReconnectBackoff::new(),
            uses_listen_key,
            synced: false,
            resync_running: false,
            resync_again: false,
            now: Duration::ZERO,
            last_resync_start: None,
            auth_failed: false,
            unavailable: false,
            session_voided: false,
            keepalive_due: None,
            keepalive_running: false,
        };
        let cmds = (0..legs)
            .map(|leg| Command::Connect {
                leg,
                after: Duration::ZERO,
            })
            .collect();
        (m, cmds)
    }

    /// Tell the machine the time. The driver calls it before every input so the minimum spacing
    /// of REST reads can be judged without a clock in here.
    pub fn observe(&mut self, now: Duration) {
        self.now = self.now.max(now);
    }

    pub fn state(&self) -> StreamState {
        if self.auth_failed {
            StreamState::AuthFailed
        } else if self.unavailable {
            StreamState::Unavailable
        } else if self.legs.contains(&Leg::Down) {
            StreamState::Reconnecting
        } else if !self.synced {
            StreamState::Resyncing
        } else {
            StreamState::Connected
        }
    }

    fn stopped(&self) -> bool {
        self.auth_failed || self.unavailable
    }

    /// Connection `leg` is up (logged in and subscribed, or connected with its key). Once every
    /// connection is up the REST read starts.
    pub fn leg_up(&mut self, leg: usize, now: Duration) -> Vec<Command> {
        self.observe(now);
        if self.stopped() || leg >= self.legs.len() {
            return Vec::new();
        }
        self.legs[leg] = Leg::Up;
        self.session_voided = false;
        if self.legs.contains(&Leg::Down) {
            return Vec::new();
        }
        // Events may have been missed while any connection was down: nothing is trusted
        // until the REST read has finished.
        self.synced = false;
        if self.uses_listen_key {
            self.keepalive_due = Some(now + KEEPALIVE_EVERY);
            self.keepalive_running = false;
        }
        self.start_resync(Duration::ZERO)
    }

    fn start_resync(&mut self, after: Duration) -> Vec<Command> {
        self.synced = false;
        if self.resync_running {
            self.resync_again = true;
            Vec::new()
        } else {
            self.resync_running = true;
            let spacing = self.last_resync_start.map_or(Duration::ZERO, |t| {
                (t + MIN_RESYNC_INTERVAL).saturating_sub(self.now)
            });
            let after = after.max(spacing);
            self.last_resync_start = Some(self.now + after);
            vec![Command::Resync { after }]
        }
    }

    /// Connection `leg` ended or failed to start.
    pub fn leg_down(&mut self, leg: usize, cause: DownCause) -> Vec<Command> {
        if self.stopped() || leg >= self.legs.len() {
            return Vec::new();
        }
        let mut cmds = Vec::new();
        match cause {
            DownCause::Auth => {
                self.auth_failed = true;
                self.synced = false;
                self.keepalive_due = None;
                return vec![
                    Command::InvalidateSession(InvalidReason::StreamAuthFailed),
                    Command::Stop,
                ];
            }
            DownCause::Unavailable => {
                self.unavailable = true;
                self.synced = false;
                self.keepalive_due = None;
                return vec![Command::Stop];
            }
            DownCause::Keychain => {
                if !self.session_voided {
                    self.session_voided = true;
                    cmds.push(Command::InvalidateSession(
                        InvalidReason::KeychainUnavailable,
                    ));
                }
            }
            DownCause::ClockSkew => cmds.push(Command::SyncClock),
            DownCause::Network | DownCause::Proxy | DownCause::Expired => {}
        }
        self.legs[leg] = Leg::Down;
        self.synced = false;
        self.keepalive_due = None;
        self.keepalive_running = false;
        let after = self.backoffs[leg].delay();
        self.backoffs[leg].record_failure();
        cmds.push(Command::Connect { leg, after });
        cmds
    }

    /// Something said local state may be wrong, or the listenKey is gone.
    pub fn on_event(&mut self, event: &super::events::PrivateEvent) -> Vec<Command> {
        use super::events::PrivateEvent as E;
        if self.stopped() {
            return Vec::new();
        }
        match event {
            E::ListenKeyExpired | E::StreamTerminated => self.leg_down(0, DownCause::Expired),
            E::ResyncRequired(_) => {
                if self.legs.contains(&Leg::Down) {
                    // Everything is read again when the connections are back.
                    self.synced = false;
                    Vec::new()
                } else {
                    self.start_resync(Duration::ZERO)
                }
            }
            _ => Vec::new(),
        }
    }

    /// The REST read finished. `ok == false`: try again after a backoff.
    pub fn resync_done(&mut self, ok: bool) -> Vec<Command> {
        self.resync_running = false;
        if self.stopped() {
            return Vec::new();
        }
        if self.legs.contains(&Leg::Down) {
            // A connection dropped meanwhile: the read is repeated once they are all back.
            self.resync_again = false;
            self.synced = false;
            return Vec::new();
        }
        if !ok {
            self.resync_again = false;
            let after = self.resync_backoff.delay();
            self.resync_backoff.record_failure();
            return self.start_resync(after);
        }
        if self.resync_again {
            self.resync_again = false;
            return self.start_resync(Duration::ZERO);
        }
        self.synced = true;
        self.resync_backoff.record_success();
        for b in &mut self.backoffs {
            b.record_success();
        }
        Vec::new()
    }

    /// Time passed: extend the listenKey when it is due.
    pub fn tick(&mut self, now: Duration) -> Vec<Command> {
        self.observe(now);
        match self.keepalive_due {
            Some(due) if !self.stopped() && !self.keepalive_running && now >= due => {
                self.keepalive_running = true;
                vec![Command::KeepAlive]
            }
            _ => Vec::new(),
        }
    }

    pub fn keepalive_done(&mut self, ok: bool, now: Duration) -> Vec<Command> {
        self.observe(now);
        self.keepalive_running = false;
        if self.stopped() {
            return Vec::new();
        }
        if ok {
            self.keepalive_due = Some(now + KEEPALIVE_EVERY);
            Vec::new()
        } else {
            // The key is gone or cannot be extended: make a new one, reconnect, read again.
            self.leg_down(0, DownCause::Expired)
        }
    }

    /// The machine woke from sleep: every connection is treated as dropped (condition 51),
    /// the clock is measured again, and nothing waits for a backoff.
    pub fn wake(&mut self) -> Vec<Command> {
        if self.stopped() {
            return Vec::new();
        }
        let mut cmds = vec![Command::SyncClock];
        for leg in 0..self.legs.len() {
            self.legs[leg] = Leg::Down;
            self.backoffs[leg] = ReconnectBackoff::new();
            cmds.push(Command::Connect {
                leg,
                after: Duration::ZERO,
            });
        }
        self.synced = false;
        self.resync_again = self.resync_running;
        self.keepalive_due = None;
        self.keepalive_running = false;
        cmds
    }

    /// The account was deleted or disabled.
    pub fn shutdown(&mut self) -> Vec<Command> {
        let mut cmds = Vec::new();
        if self.uses_listen_key {
            cmds.push(Command::DeleteKey);
        }
        cmds.push(Command::Stop);
        self.unavailable = true;
        self.keepalive_due = None;
        cmds
    }
}

#[cfg(test)]
mod tests {
    use super::super::events::{PrivateEvent, ResyncReason};
    use super::*;

    const T0: Duration = Duration::from_secs(1000);

    fn connected(legs: usize, key: bool) -> Machine {
        let (mut m, cmds) = Machine::start(legs, key);
        assert_eq!(cmds.len(), legs);
        for leg in 0..legs {
            m.leg_up(leg, T0);
        }
        assert_eq!(m.state(), StreamState::Resyncing);
        assert!(m.resync_done(true).is_empty());
        assert_eq!(m.state(), StreamState::Connected);
        m
    }

    fn connect_delay(cmds: &[Command], leg: usize) -> Duration {
        cmds.iter()
            .find_map(|c| match c {
                Command::Connect { leg: l, after } if *l == leg => Some(*after),
                _ => None,
            })
            .expect("a connect command")
    }

    #[test]
    fn matrix_of_states_and_actions() {
        use StreamState::*;
        use TradeAction::*;
        let states = [Connected, Reconnecting, Resyncing, AuthFailed, Unavailable];
        let actions = [NewEntry, DragModify, Cancel, Close, CancelAll, CloseAll];
        for s in states {
            for a in actions {
                let expected = match a {
                    NewEntry | DragModify => s == Connected,
                    Cancel | Close | CancelAll | CloseAll => true,
                };
                assert_eq!(allows(s, a), expected, "{s:?} x {a:?}");
            }
            assert_eq!(lists_may_be_stale(s), s != Connected);
            assert_eq!(one_cancels_other_paused(s), s != Connected);
        }
    }

    #[test]
    fn starts_reconnecting_then_resyncing_then_connected() {
        let (mut m, cmds) = Machine::start(1, true);
        assert_eq!(
            cmds,
            vec![Command::Connect {
                leg: 0,
                after: Duration::ZERO
            }]
        );
        assert_eq!(m.state(), StreamState::Reconnecting);
        let cmds = m.leg_up(0, T0);
        assert_eq!(
            cmds,
            vec![Command::Resync {
                after: Duration::ZERO
            }]
        );
        assert_eq!(m.state(), StreamState::Resyncing);
        // New entries stay refused until the REST read is done.
        assert!(!allows(m.state(), TradeAction::NewEntry));
        m.resync_done(true);
        assert_eq!(m.state(), StreamState::Connected);
        assert!(allows(m.state(), TradeAction::NewEntry));
    }

    #[test]
    fn authentication_failure_stops_and_never_reconnects() {
        let mut m = connected(2, false);
        let cmds = m.leg_down(0, DownCause::Auth);
        assert_eq!(
            cmds,
            vec![
                Command::InvalidateSession(InvalidReason::StreamAuthFailed),
                Command::Stop
            ]
        );
        assert_eq!(m.state(), StreamState::AuthFailed);
        assert!(!cmds.iter().any(|c| matches!(c, Command::Connect { .. })));
        // Nothing revives it by itself: not a later drop, not a wake, not an event.
        assert!(m.leg_down(1, DownCause::Network).is_empty());
        assert!(m.wake().is_empty());
        assert!(m.leg_up(0, T0).is_empty());
        assert!(m.on_event(&PrivateEvent::ListenKeyExpired).is_empty());
        assert_eq!(m.state(), StreamState::AuthFailed);
    }

    #[test]
    fn network_errors_back_off_with_a_bounded_growing_delay() {
        let mut m = connected(1, true);
        let mut previous_max = Duration::ZERO;
        for i in 0..10 {
            let cmds = m.leg_down(0, DownCause::Network);
            let d = connect_delay(&cmds, 0);
            assert!(d <= Duration::from_secs(30), "capped: {d:?}");
            // 500 ms doubling, ±25% jitter.
            let nominal = Duration::from_millis(500).saturating_mul(1u32 << i.min(6));
            let nominal = nominal.min(Duration::from_secs(30));
            assert!(
                d >= nominal.mul_f32(0.74).min(Duration::from_secs(30)),
                "{i}: {d:?}"
            );
            assert!(
                d <= nominal.mul_f32(1.26).min(Duration::from_secs(30)),
                "{i}: {d:?}"
            );
            assert!(nominal >= previous_max);
            previous_max = nominal;
            assert_eq!(m.state(), StreamState::Reconnecting);
        }
        // A genuine success (connected and synchronised) resets it.
        m.leg_up(0, T0);
        m.resync_done(true);
        let cmds = m.leg_down(0, DownCause::Network);
        assert!(connect_delay(&cmds, 0) <= Duration::from_millis(700));
    }

    #[test]
    fn jitter_actually_varies_the_delay() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..40 {
            let (mut m, _) = Machine::start(1, false);
            m.leg_up(0, T0);
            m.resync_done(true);
            seen.insert(connect_delay(&m.leg_down(0, DownCause::Network), 0));
        }
        assert!(seen.len() > 5);
    }

    #[test]
    fn okx_legs_back_off_on_their_own() {
        let mut m = connected(2, false);
        let first = m.leg_down(0, DownCause::Network);
        for _ in 0..5 {
            m.leg_up(0, T0);
            m.leg_down(0, DownCause::Network);
        }
        // Leg 1 never failed: its first retry is still the short one.
        let second = m.leg_down(1, DownCause::Network);
        assert!(connect_delay(&second, 1) <= Duration::from_millis(700));
        assert!(connect_delay(&first, 0) <= Duration::from_millis(700));
        assert_eq!(m.state(), StreamState::Reconnecting);
    }

    #[test]
    fn okx_needs_both_connections_before_it_resyncs() {
        let (mut m, _) = Machine::start(2, false);
        assert!(m.leg_up(0, T0).is_empty());
        assert_eq!(m.state(), StreamState::Reconnecting);
        assert_eq!(
            m.leg_up(1, T0),
            vec![Command::Resync {
                after: Duration::ZERO
            }]
        );
        assert_eq!(m.state(), StreamState::Resyncing);
        // One connection dropping while the REST read runs sends the state back.
        m.leg_down(1, DownCause::Network);
        assert!(m.resync_done(true).is_empty());
        assert_eq!(m.state(), StreamState::Reconnecting);
        // When it is back (after the minimum spacing), the read runs again.
        assert_eq!(
            m.leg_up(1, T0 + Duration::from_secs(10)),
            vec![Command::Resync {
                after: Duration::ZERO
            }]
        );
        m.resync_done(true);
        assert_eq!(m.state(), StreamState::Connected);
    }

    #[test]
    fn listen_key_is_extended_on_a_schedule_below_thirty_minutes() {
        assert!(KEEPALIVE_EVERY <= Duration::from_secs(30 * 60));
        let mut m = connected(1, true);
        assert!(
            m.tick(T0 + KEEPALIVE_EVERY - Duration::from_secs(1))
                .is_empty()
        );
        assert_eq!(m.tick(T0 + KEEPALIVE_EVERY), vec![Command::KeepAlive]);
        // Not asked again while one is running.
        assert!(
            m.tick(T0 + KEEPALIVE_EVERY + Duration::from_secs(5))
                .is_empty()
        );
        let t1 = T0 + KEEPALIVE_EVERY + Duration::from_secs(10);
        assert!(m.keepalive_done(true, t1).is_empty());
        assert!(
            m.tick(t1 + KEEPALIVE_EVERY - Duration::from_secs(1))
                .is_empty()
        );
        assert_eq!(m.tick(t1 + KEEPALIVE_EVERY), vec![Command::KeepAlive]);
        assert_eq!(m.state(), StreamState::Connected);
    }

    #[test]
    fn a_failed_extension_rebuilds_the_key_reconnects_and_resyncs() {
        let mut m = connected(1, true);
        m.tick(T0 + KEEPALIVE_EVERY);
        let cmds = m.keepalive_done(false, T0 + KEEPALIVE_EVERY);
        assert!(connect_delay(&cmds, 0) > Duration::ZERO);
        assert_eq!(m.state(), StreamState::Reconnecting);
        // Entries are refused from the moment the key is lost until the read is done.
        assert!(!allows(m.state(), TradeAction::NewEntry));
        assert_eq!(m.leg_up(0, T0 + KEEPALIVE_EVERY).len(), 1);
        assert_eq!(m.state(), StreamState::Resyncing);
        m.resync_done(true);
        assert_eq!(m.state(), StreamState::Connected);
    }

    #[test]
    fn a_listen_key_expired_event_does_the_same() {
        let mut m = connected(1, true);
        let cmds = m.on_event(&PrivateEvent::ListenKeyExpired);
        assert!(matches!(cmds.as_slice(), [Command::Connect { leg: 0, .. }]));
        assert_eq!(m.state(), StreamState::Reconnecting);
        // No extension is scheduled while there is no key.
        assert!(m.tick(T0 + KEEPALIVE_EVERY * 3).is_empty());
    }

    #[test]
    fn a_resync_request_while_one_runs_runs_again_afterwards() {
        let mut m = connected(1, false);
        m.observe(T0 + Duration::from_secs(10));
        let ev = PrivateEvent::ResyncRequired(ResyncReason::UnknownOrderStatus);
        assert_eq!(
            m.on_event(&ev),
            vec![Command::Resync {
                after: Duration::ZERO
            }]
        );
        assert_eq!(m.state(), StreamState::Resyncing);
        // Any number of requests while it runs are merged into one more read.
        assert!(m.on_event(&ev).is_empty());
        assert!(m.on_event(&ev).is_empty());
        m.observe(T0 + Duration::from_secs(20));
        assert_eq!(
            m.resync_done(true),
            vec![Command::Resync {
                after: Duration::ZERO
            }]
        );
        assert_eq!(m.state(), StreamState::Resyncing);
        assert!(m.resync_done(true).is_empty());
        assert_eq!(m.state(), StreamState::Connected);
    }

    #[test]
    fn reads_are_spaced_by_the_minimum_interval_and_requests_are_merged() {
        let mut m = connected(1, false);
        let ev = PrivateEvent::ResyncRequired(ResyncReason::MalformedFrame);
        // The first read started at T0; a request half a second later waits the rest.
        m.observe(T0 + Duration::from_millis(500));
        assert_eq!(
            m.on_event(&ev),
            vec![Command::Resync {
                after: MIN_RESYNC_INTERVAL - Duration::from_millis(500)
            }]
        );
        // A flood of requests while it is pending makes one repeat, not one per request.
        for _ in 0..100 {
            assert!(m.on_event(&ev).is_empty());
        }
        // The read finished quickly: the repeat still keeps the spacing from the last start.
        m.observe(T0 + MIN_RESYNC_INTERVAL + Duration::from_millis(100));
        assert_eq!(
            m.resync_done(true),
            vec![Command::Resync {
                after: MIN_RESYNC_INTERVAL - Duration::from_millis(100)
            }]
        );
        assert!(m.on_event(&ev).is_empty());
        m.observe(T0 + Duration::from_secs(60));
        assert_eq!(
            m.resync_done(true),
            vec![Command::Resync {
                after: Duration::ZERO
            }]
        );
        assert!(m.resync_done(true).is_empty());
        assert_eq!(m.state(), StreamState::Connected);
    }

    #[test]
    fn a_failed_resync_is_retried_after_a_backoff_and_stays_unsynced() {
        let mut m = connected(1, false);
        m.on_event(&PrivateEvent::ResyncRequired(ResyncReason::MalformedFrame));
        let cmds = m.resync_done(false);
        let [Command::Resync { after }] = cmds.as_slice() else {
            panic!("{cmds:?}")
        };
        assert!(*after > Duration::ZERO && *after <= Duration::from_secs(30));
        assert_eq!(m.state(), StreamState::Resyncing);
        m.resync_done(true);
        assert_eq!(m.state(), StreamState::Connected);
    }

    #[test]
    fn waking_from_sleep_is_a_disconnect_that_measures_the_clock_and_reconnects_at_once() {
        let mut m = connected(2, false);
        let cmds = m.wake();
        assert_eq!(cmds[0], Command::SyncClock);
        for leg in 0..2 {
            assert_eq!(connect_delay(&cmds, leg), Duration::ZERO);
        }
        assert_eq!(m.state(), StreamState::Reconnecting);
        assert!(!allows(m.state(), TradeAction::NewEntry));
    }

    #[test]
    fn keychain_failure_voids_the_session_once_and_keeps_retrying() {
        let mut m = connected(1, false);
        let cmds = m.leg_down(0, DownCause::Keychain);
        assert_eq!(
            cmds[0],
            Command::InvalidateSession(InvalidReason::KeychainUnavailable)
        );
        assert!(matches!(cmds[1], Command::Connect { .. }));
        let again = m.leg_down(0, DownCause::Keychain);
        assert!(
            !again
                .iter()
                .any(|c| matches!(c, Command::InvalidateSession(_)))
        );
        assert_eq!(m.state(), StreamState::Reconnecting);
    }

    #[test]
    fn clock_skew_measures_the_clock_before_the_retry() {
        let mut m = connected(1, false);
        let cmds = m.leg_down(0, DownCause::ClockSkew);
        assert_eq!(cmds[0], Command::SyncClock);
        assert!(matches!(cmds[1], Command::Connect { .. }));
    }

    #[test]
    fn unavailable_stops_without_connecting_and_refuses_entries() {
        let (mut m, _) = Machine::start(1, false);
        assert_eq!(m.leg_down(0, DownCause::Unavailable), vec![Command::Stop]);
        assert_eq!(m.state(), StreamState::Unavailable);
        assert!(!allows(m.state(), TradeAction::NewEntry));
        assert!(allows(m.state(), TradeAction::Cancel));
        assert!(m.wake().is_empty());
    }

    #[test]
    fn shutdown_closes_the_listen_key_of_a_futures_account_only() {
        let mut m = connected(1, true);
        assert_eq!(m.shutdown(), vec![Command::DeleteKey, Command::Stop]);
        let mut o = connected(2, false);
        assert_eq!(o.shutdown(), vec![Command::Stop]);
    }
}
