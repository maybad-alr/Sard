//! Where the mail app secret lives — and the only place it is allowed to live.
//!
//! The trait exists so that the rule can be TESTED rather than asserted: the tests drive `save` and
//! `send_book` with a store they can inspect, and prove the secret never reaches the database or the
//! message. A rule with no seam is a rule that holds until someone adds a convenience.

/// The OS credential store.
pub trait SecretStore {
    /// The saved secret: `Ok(None)` when there is none, `Err` when the store itself is unavailable.
    /// Those two are different answers and the caller treats them differently — "not set up yet" and
    /// "this system will not let Sard keep a secret" are not the same thing to tell a reader.
    fn secret(&self, account: &str) -> Result<Option<String>, String>;
    /// Store it. Failing here must stop the setup: see the module note in `kindle`.
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

/// A message for the reader, never the platform's raw dump.
fn describe(error: keyring::Error) -> String {
    match error {
        keyring::Error::NoDefaultStore => "no credential store on this platform".to_string(),
        other => other.to_string(),
    }
}
