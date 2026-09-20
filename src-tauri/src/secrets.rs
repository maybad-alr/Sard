//! Where secrets live — and the only place they are allowed to live.
//!
//! SHARED, AND DELIBERATELY NOT PART OF ANY ONE FEATURE. Two features now hold a secret: the mail
//! secret that sends a book to a Kindle, and the sync account's refresh token. Two copies of a rule
//! is one rule and one stale copy, so there is one store, one seam, and one place a reader can look.
//!
//! The trait exists so the rule can be TESTED rather than asserted: the tests drive each feature with
//! a store they can inspect, and prove the secret never reaches the database or the message.

/// The OS credential store, or the honest statement that this platform has none yet.
pub trait SecretStore {
    /// The saved secret: `Ok(None)` when there is none, `Err` when the store itself is unavailable.
    /// Those two are different answers and the caller treats them differently — "not set up yet" and
    /// "this system will not let Sard keep a secret" are not the same thing to tell a reader.
    fn secret(&self, account: &str) -> Result<Option<String>, String>;
    /// Store it. Failing here must stop the caller's setup: see the note on `OsStore` below.
    fn put(&self, account: &str, value: &str) -> Result<(), String>;
    fn forget(&self, account: &str) -> Result<(), String>;
}

/// The real store: Windows Credential Manager, macOS Keychain, or the Secret Service on Linux.
///
/// It is chosen by the `keyring` crate's `v1` mode, which manages one platform-appropriate store. The
/// service name is the product's, so a reader who opens their credential manager finds an entry they
/// recognise and can remove it by hand if they would rather not use Sard's own switch.
pub struct OsStore;

const SERVICE: &str = "Sard";

/// The one sentence every platform without a store gives, so the interface has one thing to translate.
pub const NO_STORE: &str = "this platform has no credential store yet";

/// DESKTOP ONLY, AND IT SAYS SO RATHER THAN PRETENDING.
///
/// Android's keystore is reached through a bridge that ships with the mobile project, which this tree
/// does not have yet, and this crate's credential-store dependency has no backend there. A store that
/// quietly kept the value in a file instead would be a plaintext fallback wearing the same name — and
/// the whole point of this module is that there is no such fallback. So on a platform without a store
/// the answer is that there is none, and the feature that needs one tells the reader.
#[cfg(desktop)]
impl SecretStore for OsStore {
    fn secret(&self, account: &str) -> Result<Option<String>, String> {
        let entry = keyring::Entry::new(SERVICE, account).map_err(describe)?;
        match entry.get_password() {
            Ok(value) => Ok(Some(value)),
            // The crate's way of saying "no such entry", which is not a failure: it is the state of a
            // reader who has not finished setting this up.
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(describe(e)),
        }
    }

    fn put(&self, account: &str, value: &str) -> Result<(), String> {
        keyring::Entry::new(SERVICE, account)
            .and_then(|entry| entry.set_password(value))
            .map_err(describe)
    }

    fn forget(&self, account: &str) -> Result<(), String> {
        let entry = keyring::Entry::new(SERVICE, account).map_err(describe)?;
        match entry.delete_credential() {
            Ok(()) => Ok(()),
            // Deleting what is not there satisfies the request; a reader who never stored anything is
            // not in an error state for turning the feature off.
            Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(describe(e)),
        }
    }
}

#[cfg(not(desktop))]
impl SecretStore for OsStore {
    fn secret(&self, _: &str) -> Result<Option<String>, String> {
        Err(NO_STORE.into())
    }
    fn put(&self, _: &str, _: &str) -> Result<(), String> {
        Err(NO_STORE.into())
    }
    fn forget(&self, _: &str) -> Result<(), String> {
        Err(NO_STORE.into())
    }
}

/// A message for the reader, never the platform's raw dump.
#[cfg(desktop)]
fn describe(error: keyring::Error) -> String {
    match error {
        keyring::Error::NoDefaultStore => NO_STORE.to_string(),
        other => other.to_string(),
    }
}
