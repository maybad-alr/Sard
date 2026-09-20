//! SEND A BOOK TO KINDLE — one book, by email, from inside Sard.
//!
//! # Why email, and not an API
//!
//! Amazon publishes NO API for this. "Send to Kindle" exists as a web page, as apps for Windows,
//! macOS, iOS and Android, as a browser extension, and as EMAIL — and the email is the only one of
//! those a third-party application can drive. (Checked against Amazon's own help pages, 2026-09-20.)
//! The routes people describe instead — driving the Kindle Cloud Reader's session, reading a Kindle's
//! internal store — are reverse-engineered, break on Amazon's schedule, and are excluded by Amazon's
//! own terms of use. Sard does not ship those, so email is the honest feature rather than a clever one.
//!
//! The same research is why there is no "read the reader's Kindle position" here: Amazon exposes no
//! way to read OR write a reading position, so the only Kindle feature that can exist is one-way.
//!
//! # What the reader sets up, once
//!
//! 1. Add their own sending address to Amazon's approved list — Amazon accepts a document only from an
//!    address the account has approved, which is why this feature needs a mail account at all;
//! 2. paste their Send-to-Kindle address (the `@kindle.com` one from Manage Your Content and Devices);
//! 3. paste a mail APP PASSWORD. Not their account password: providers that require 2FA issue a
//!    separate app password precisely so an application never holds the real one.
//!
//! # The secret, and what is promised about it
//!
//! The app password is the first secret Sard has ever handled, and it is treated as one:
//!
//!   · it lives ONLY in the OS credential store — Windows Credential Manager, macOS Keychain, Secret
//!     Service. There is no `password` field in this module's config type, so it cannot be written to
//!     `sard.db` by accident even if someone later tries to;
//!   · if that store is unavailable the feature REFUSES to save. There is no plaintext fallback,
//!     because a fallback is what turns "we never write your password to a file" into a promise that
//!     was true until it was inconvenient;
//!   · it never enters the message and never a log line — both are asserted by tests.
//!
//! # What the email does NOT carry
//!
//! The book's file, two addresses and one fixed line of body text. No reading position, no notes, no
//! library list, no device id: nothing about what the reader has read. Amazon needs a file, not a
//! history of one.
//!
//! # Desktop only, for now, and deliberately
//!
//! The dependencies are gated in `Cargo.toml` (see the note there). Android's credential store needs a
//! Kotlin bridge that ships with the mobile project, so on Android this module is not compiled at all
//! rather than compiled and broken — a switch that cannot work is worse than one that is not there.

pub mod mail;
pub mod secrets;

#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use mail::{Mailer, Outgoing};
use secrets::SecretStore;

/// The credential-store account name the password is filed under.
const SECRET_ACCOUNT: &str = "kindle.smtp_password";

/// The non-secret half of the setup, one `settings` row each. Nothing here is sensitive: an address is
/// something the reader reads out to support and pastes into other apps.
const KEY_ADDRESS: &str = "kindle.address";
const KEY_FROM: &str = "kindle.from";
const KEY_HOST: &str = "kindle.smtp_host";
const KEY_PORT: &str = "kindle.smtp_port";
const KEY_USER: &str = "kindle.smtp_user";

/// The default submission port. 587 is the one that speaks STARTTLS, which is the only way this
/// module sends — see `mail::SmtpMailer` for why the implicit-TLS port is not offered.
pub const DEFAULT_PORT: u16 = 587;

/// The largest book this will attach.
///
/// AMAZON'S OWN LIMIT IS NOT THE BINDING ONE. Amazon accepts 50 MB per email, but the message goes out
/// through the READER'S provider and that ceiling is lower: Gmail refuses a message over 25 MB and
/// silently replaces the attachment with a Drive LINK, which is useless to Amazon. base64 inflates an
/// attachment by a third on the wire, so the file itself must stay under 25 × 3/4 ≈ 18.75 MB — and it
/// is capped at 18 MB to leave room for headers and the multipart wrapper.
///
/// The arithmetic is the reason this is not 20 MB, which is where a first guess landed and which Gmail
/// would have refused after the reader had already waited for the upload.
pub const MAX_BOOK_BYTES: u64 = 18 * 1024 * 1024;

/// The one line of body text. Fixed, and carrying nothing — see the module note.
pub const BODY: &str = "Sent from Sard.";

/// What the reader has to have set up. NO PASSWORD FIELD, on purpose and by construction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KindleConfig {
    /// The reader's Send-to-Kindle address.
    pub address: String,
    /// The reader's own mailbox: Amazon accepts a document only from an approved address, so the
    /// sender and the SMTP account are the reader's.
    pub from: String,
    pub smtp_host: String,
    pub smtp_port: u16,
    /// The SMTP login. Often the same as `from`, but not always, so it is its own field.
    pub smtp_user: String,
}

impl KindleConfig {
    /// Whether this could be sent with — the shallow checks only.
    ///
    /// Deliberately shallow: an address that passes can still be wrong, and only the mail server can
    /// say so. What this refuses is the class a reader actually gets wrong — a missing `@`, a pasted
    /// space, an empty box — BEFORE a connection is opened for it and before anything is stored.
    pub fn validate(&self) -> Result<(), KindleError> {
        if !looks_like_address(&self.address) {
            return Err(KindleError::Invalid("kindle.err.address"));
        }
        if !looks_like_address(&self.from) {
            return Err(KindleError::Invalid("kindle.err.from"));
        }
        if self.smtp_host.trim().is_empty() || self.smtp_host.contains(char::is_whitespace) {
            return Err(KindleError::Invalid("kindle.err.host"));
        }
        if self.smtp_port == 0 {
            return Err(KindleError::Invalid("kindle.err.port"));
        }
        Ok(())
    }
}

/// Why a send or a setup did not happen. `code()` returns the stable key the interface translates —
/// the same shape the rest of the IPC seam uses (`reveal.err.gone`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KindleError {
    /// Nothing saved yet.
    NotConfigured,
    /// Unusable as configured; the key says which field.
    Invalid(&'static str),
    /// The credential store could not be reached. Nothing is stored and nothing is sent.
    NoStore(String),
    /// No password saved for this account.
    NoPassword,
    /// No such book in this library.
    NoBook,
    /// The book is in the library but its file is not on disk.
    BookMissing,
    /// Bigger than `MAX_BOOK_BYTES`, refused before any connection.
    TooLarge { bytes: u64, max: u64 },
    /// The mail server refused, or could not be reached.
    Smtp(String),
}

impl KindleError {
    pub fn code(&self) -> String {
        match self {
            KindleError::NotConfigured => "kindle.err.notConfigured".into(),
            KindleError::Invalid(key) => (*key).into(),
            KindleError::NoStore(_) => "kindle.err.noStore".into(),
            KindleError::NoPassword => "kindle.err.noPassword".into(),
            KindleError::NoBook => "kindle.err.noBook".into(),
            KindleError::BookMissing => "kindle.err.bookMissing".into(),
            KindleError::TooLarge { .. } => "kindle.err.tooLarge".into(),
            KindleError::Smtp(_) => "kindle.err.smtp".into(),
        }
    }
}

/// What a successful send did — enough for the interface to say something true.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Sent {
    /// The name the document will appear under in the Kindle library.
    pub attachment: String,
    pub bytes: u64,
}

/// The saved setup, or `None` when the reader has never set this up.
pub fn load(conn: &Connection) -> rusqlite::Result<Option<KindleConfig>> {
    let Some(address) = crate::settings::get(conn, KEY_ADDRESS)? else { return Ok(None) };
    let Some(from) = crate::settings::get(conn, KEY_FROM)? else { return Ok(None) };
    let Some(smtp_host) = crate::settings::get(conn, KEY_HOST)? else { return Ok(None) };
    Ok(Some(KindleConfig {
        address,
        from,
        smtp_host,
        smtp_port: crate::settings::get(conn, KEY_PORT)?
            .and_then(|p| p.parse().ok())
            .unwrap_or(DEFAULT_PORT),
        smtp_user: crate::settings::get(conn, KEY_USER)?.unwrap_or_default(),
    }))
}

/// Save the setup — INCLUDING the password, or nothing at all.
///
/// THE PASSWORD IS WRITTEN FIRST, and that order is the whole point: it is the part that can fail, and
/// when it does the reader gets an error instead of a half-saved setup on a feature that cannot send.
/// The address is checked before either write, so an unusable address never reaches the credential
/// store.
pub fn save(
    conn: &Connection,
    config: &KindleConfig,
    password: &str,
    secrets: &dyn SecretStore,
) -> Result<(), KindleError> {
    config.validate()?;
    if password.is_empty() {
        return Err(KindleError::Invalid("kindle.err.password"));
    }
    secrets.put(SECRET_ACCOUNT, password).map_err(KindleError::NoStore)?;

    // The non-secret half. `settings::set` is the app's only writer of this table (and the one that
    // stamps a book's dirty mark) — going through it keeps that true.
    let port = config.smtp_port.to_string();
    for (key, value) in [
        (KEY_ADDRESS, config.address.trim()),
        (KEY_FROM, config.from.trim()),
        (KEY_HOST, config.smtp_host.trim()),
        (KEY_PORT, port.as_str()),
        (KEY_USER, config.smtp_user.trim()),
    ] {
        crate::settings::set(conn, key, value).map_err(|e| KindleError::NoStore(e.to_string()))?;
    }
    Ok(())
}

/// Forget the feature: the address and the password both.
///
/// A reader who turns this off leaves NO secret behind. That is only possible because the password was
/// never in the database — the address rows and the credential are removed independently, and neither
/// can orphan the other.
pub fn forget(conn: &Connection, secrets: &dyn SecretStore) -> Result<(), KindleError> {
    secrets.forget(SECRET_ACCOUNT).map_err(KindleError::NoStore)?;
    for key in [KEY_ADDRESS, KEY_FROM, KEY_HOST, KEY_PORT, KEY_USER] {
        conn.execute("DELETE FROM settings WHERE key = ?1", [key])
            .map_err(|e| KindleError::NoStore(e.to_string()))?;
    }
    Ok(())
}

/// Send one book to the reader's Kindle address.
pub fn send_book(
    conn: &Connection,
    book_id: &str,
    config: &KindleConfig,
    secrets: &dyn SecretStore,
    mailer: &dyn Mailer,
) -> Result<Sent, KindleError> {
    config.validate()?;

    let (path, title, author) = book_file(conn, book_id)?;
    let bytes = std::fs::read(&path).map_err(|_| KindleError::BookMissing)?;
    let size = bytes.len() as u64;
    if size > MAX_BOOK_BYTES {
        // Refused HERE, before the credential is read and before any connection: the reader should not
        // be asked for a password to be told the file is too big.
        return Err(KindleError::TooLarge { bytes: size, max: MAX_BOOK_BYTES });
    }

    let password = secrets
        .secret(SECRET_ACCOUNT)
        .map_err(KindleError::NoStore)?
        .ok_or(KindleError::NoPassword)?;

    let attachment = attachment_name(&title, &author, &path);
    let outgoing = Outgoing {
        from: config.from.trim().to_string(),
        to: config.address.trim().to_string(),
        subject: subject_for(&title),
        body: BODY.to_string(),
        content_type: content_type_for(&path).to_string(),
        attachment_name: attachment.clone(),
        attachment: bytes,
    };
    mailer.send(config, &password, &outgoing).map_err(KindleError::Smtp)?;

    Ok(Sent { attachment, bytes: size })
}

/// The book's managed file, and the metadata the attachment name is built from.
fn book_file(conn: &Connection, book_id: &str) -> Result<(PathBuf, String, String), KindleError> {
    let row = conn
        .query_row(
            // `title`/`author` are nullable: a book imported without metadata is still a book, and the
            // name it travels under falls back rather than failing the send.
            "SELECT file_path, COALESCE(title, ''), COALESCE(author, '') FROM books WHERE id = ?1",
            [book_id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)),
        )
        .optional()
        .map_err(|e| KindleError::Smtp(e.to_string()))?;
    let Some((path, title, author)) = row else { return Err(KindleError::NoBook) };
    Ok((PathBuf::from(path), title, author))
}

/// The name the document will carry in the Kindle library.
///
/// THE MANAGED FILE'S OWN NAME CANNOT BE USED. Sard stores its copy as `<book id>.epub` — the original
/// filename is gone by design, and the repository says so where it explains that the database is the
/// single source of a book's name. Attaching that name would file the reader's book in their Kindle
/// library under a 64-character hash. Amazon derives the document's title from THIS name, so it is
/// built from the book's own metadata and stripped of everything a filename cannot contain.
fn attachment_name(title: &str, author: &str, path: &Path) -> String {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("epub");
    let title = clean_for_filename(title, "book");
    let author = clean_for_filename(author, "");
    let stem = if author.is_empty() { title } else { format!("{title} - {author}") };
    format!("{}.{}", truncate_chars(&stem, 120), ext)
}

/// Strip what a filename may not contain, and fall back when nothing is left.
///
/// The separators matter most: a title containing `/` would turn into a path in whatever the reader's
/// mail client does with the name, and a title containing a newline would split a MIME header.
fn clean_for_filename(text: &str, fallback: &str) -> String {
    let cleaned: String = text
        .chars()
        .map(|c| if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') { ' ' } else { c })
        .collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed.trim_matches(|c: char| c == '.' || c == ' ').to_string();
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed
    }
}

/// Cut on a CHARACTER boundary, not a byte one: a byte slice through an Arabic title would panic, and
/// the titles this matters for are exactly the Arabic ones.
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    text.chars().take(max).collect()
}

fn subject_for(title: &str) -> String {
    let title = clean_for_filename(title, "Sard");
    format!("Sard — {}", truncate_chars(&title, 120))
}

/// The attachment's declared type. Only what Sard can hold: anything else travels as bytes, which is
/// what an unknown type honestly is.
fn content_type_for(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref() {
        Some("epub") => "application/epub+zip",
        Some("pdf") => "application/pdf",
        _ => "application/octet-stream",
    }
}

fn looks_like_address(candidate: &str) -> bool {
    let s = candidate.trim();
    !s.is_empty()
        && !s.contains(char::is_whitespace)
        && s.matches('@').count() == 1
        && !s.starts_with('@')
        && !s.ends_with('@')
}
