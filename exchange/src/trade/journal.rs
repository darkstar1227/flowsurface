//! The write-ahead journal of sends that may end `Unknown` (review B4, condition 60).
//!
//! An unresolved send must survive a restart, but the outcome of a send is only known after it
//! returns, and the process can end at any point before that. So the record is written **before**
//! the request goes out and removed only once the outcome is known. If the process ends, the
//! future is dropped, or the outcome is `Unknown`, the record stays and the next start locks
//! the symbol again. A record that is left behind by mistake only costs a lookup; a record that
//! was never written would unlock the symbol silently.
//!
//! Only a new entry is refused when the write fails. Reducing, closing and cancelling sends
//! always go out (conditions 60 and 72): for them the write is best effort.
//!
//! `data` implements the trait with a file (`UnknownStore`); this crate cannot depend on `data`.

use super::unknown::UnknownRecord;
use crate::account::AccountKey;

/// The journal could not be written or cleared. Carries no detail: the file system error may
/// name paths, and the caller only needs to know that the send must not go out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalError;

/// Where unresolved sends are recorded. Calls are synchronous and short; they are made from the
/// send path, so an implementation must not block on the network.
pub trait UnknownJournal {
    /// Write `record` durably. An `Err` stops a new entry (nothing goes out); a reducing or
    /// closing send goes out anyway.
    fn record(&self, record: &UnknownRecord) -> Result<(), JournalError>;
    /// Remove the record of `client_id`. Failure is tolerated by the caller (the record stays,
    /// which is fail closed).
    fn clear(&self, account: AccountKey, client_id: &str) -> Result<(), JournalError>;
}
