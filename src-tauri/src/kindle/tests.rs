//! The Kindle-by-email feature, tested on real files, a real database, and a real SMTP conversation.
//!
//! The tests that matter most here are the two that hold the PROMISE rather than the feature:
//! `the_setup_round_trips_and_the_secret_never_reaches_the_database` reads the database files off disk
//! and looks for the secret in them, and
//! `a_book_over_the_cap_is_refused_before_the_secret_is_read` proves the refusal happens before the
//! credential store is touched at all. Those are the claims a reader is asked to believe.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use rusqlite::Connection;

use super::mail::{self, Mailer, Outgoing, SmtpMailer};
use super::*;
use crate::secrets::SecretStore;

/// A distinctive value, so finding it in a file is not a coincidence.
const SECRET: &str = "app-secret-9z7q";
const BOOK: &str = "book-id-hash";

/// A payload carrying everything an email transport can ruin: bytes above 127, a NUL, a lone carriage
/// return, a bare line feed, and a line that begins with a dot — which SMTP escapes, so a transport
/// that is not encoding the bytes cannot round-trip this. It is what an EPUB actually is: a zip.
///
/// The first version of this fixture was ASCII text, and the test passed while proving nothing —
/// because an ASCII body legitimately goes out as `7bit`. The fixture is now the hostile case.
const BOOK_BYTES: &[u8] = b"PK\x03\x04\x00\x01\xff\xfebinary\rline\n.\r\nend\x00\x7f";

fn config() -> KindleConfig {
    KindleConfig {
        address: "reader@kindle.com".into(),
        from: "reader@example.com".into(),
        smtp_host: "smtp.example.com".into(),
        smtp_port: DEFAULT_PORT,
        smtp_user: "reader@example.com".into(),
    }
}

/// A real database on the migration path, with one imported book whose file exists on disk.
fn db(tag: &str, book_bytes: &[u8]) -> (Connection, PathBuf) {
    let dir = std::env::temp_dir().join(format!("sard_kindle_{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("library")).unwrap();
    let path = dir.join("library").join(format!("{BOOK}.epub"));
    std::fs::write(&path, book_bytes).unwrap();

    let conn = crate::db::open_database(&dir.join("sard.db")).unwrap();
    crate::db::migrations::run(&conn, None).unwrap();
    conn.execute(
        "INSERT INTO books(id, file_path, title, author) VALUES(?1, ?2, ?3, ?4)",
        rusqlite::params![BOOK, path.to_string_lossy(), "Book Title", "Author"],
    )
    .unwrap();
    (conn, dir)
}

/// A credential store that keeps its contents where a test can read them, and counts how often it was
/// ASKED — because "refused before the secret was read" is a claim about that number.
#[derive(Default)]
struct RecordingStore {
    entries: Mutex<HashMap<String, String>>,
    reads: AtomicUsize,
}

impl RecordingStore {
    fn len(&self) -> usize {
        self.entries.lock().unwrap().len()
    }
    fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }
    fn peek(&self, account: &str) -> Option<String> {
        self.entries.lock().unwrap().get(account).cloned()
    }
}

impl SecretStore for RecordingStore {
    fn secret(&self, account: &str) -> Result<Option<String>, String> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        Ok(self.entries.lock().unwrap().get(account).cloned())
    }
    fn put(&self, account: &str, value: &str) -> Result<(), String> {
        self.entries.lock().unwrap().insert(account.to_string(), value.to_string());
        Ok(())
    }
    fn forget(&self, account: &str) -> Result<(), String> {
        self.entries.lock().unwrap().remove(account);
        Ok(())
    }
}

/// A store that cannot keep anything — the state of a machine whose credential store is missing. The
/// feature must refuse rather than fall back to writing a secret into its own file.
struct NoStore;

impl SecretStore for NoStore {
    fn secret(&self, _: &str) -> Result<Option<String>, String> {
        Err("no credential store".into())
    }
    fn put(&self, _: &str, _: &str) -> Result<(), String> {
        Err("no credential store".into())
    }
    fn forget(&self, _: &str) -> Result<(), String> {
        Err("no credential store".into())
    }
}

#[derive(Default)]
struct RecordingMailer {
    sent: Mutex<Vec<(String, Outgoing)>>,
}

impl Mailer for RecordingMailer {
    fn send(&self, _config: &KindleConfig, secret: &str, outgoing: &Outgoing) -> Result<(), String> {
        self.sent.lock().unwrap().push((secret.to_string(), outgoing.clone()));
        Ok(())
    }
}

/// A mail server that exists for one test: it speaks just enough SMTP to accept a message and keeps
/// the bytes. Nothing in it is a product.
struct FakeSmtp {
    port: u16,
    received: Arc<Mutex<String>>,
    envelope: Arc<Mutex<Vec<String>>>,
}

impl FakeSmtp {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let received = Arc::new(Mutex::new(String::new()));
        let envelope = Arc::new(Mutex::new(Vec::new()));
        let (data_sink, envelope_sink) = (received.clone(), envelope.clone());

        std::thread::spawn(move || {
            let Ok((stream, _)) = listener.accept() else { return };
            serve(stream, data_sink, envelope_sink);
        });

        Self { port, received, envelope }
    }

    fn data(&self) -> String {
        self.received.lock().unwrap().clone()
    }

    fn envelope(&self) -> Vec<String> {
        self.envelope.lock().unwrap().clone()
    }
}

fn serve(stream: TcpStream, received: Arc<Mutex<String>>, envelope: Arc<Mutex<Vec<String>>>) {
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    let mut say = |line: &str| {
        let _ = writer.write_all(line.as_bytes());
        let _ = writer.flush();
    };

    say("220 fake ESMTP\r\n");
    let mut line = String::new();
    let mut body = String::new();
    let mut in_data = false;

    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        if in_data {
            // The terminator is a lone dot. Escaped lines (a leading "..") need no un-stuffing here:
            // the payload is base64, which cannot begin a line with a dot.
            if line.trim_end_matches(['\r', '\n']) == "." {
                in_data = false;
                say("250 2.0.0 Ok: queued\r\n");
                continue;
            }
            body.push_str(&line);
            continue;
        }

        let command = line.trim_end().to_string();
        let upper = command.to_ascii_uppercase();
        if upper.starts_with("EHLO") || upper.starts_with("HELO") {
            say("250 fake\r\n");
        } else if upper.starts_with("MAIL FROM") || upper.starts_with("RCPT TO") {
            envelope.lock().unwrap().push(command.clone());
            say("250 2.1.0 Ok\r\n");
        } else if upper.starts_with("DATA") {
            in_data = true;
            body.clear();
            say("354 End data with <CR><LF>.<CR><LF>\r\n");
        } else if upper.starts_with("QUIT") {
            say("221 2.0.0 Bye\r\n");
            break;
        } else {
            say("250 2.0.0 Ok\r\n");
        }
    }

    *received.lock().unwrap() = body;
}

/// The attachment's bytes, decoded out of a captured message. Failures carry what was actually seen,
/// because a decoder that only says "invalid" costs another run to diagnose.
///
/// It waits for the blank line that ENDS the part's headers before collecting, which is what a mail
/// client does: the headers of a part may follow the encoding declaration in any order, so collecting
/// straight after it collects headers.
fn decode_attachment(raw: &str) -> Result<Vec<u8>, String> {
    let mut encoded = String::new();
    let mut seen_encoding = false;
    let mut in_body = false;
    for line in raw.lines() {
        if !seen_encoding {
            if line.trim_end().eq_ignore_ascii_case("content-transfer-encoding: base64") {
                seen_encoding = true;
            }
            continue;
        }
        if !in_body {
            if line.trim_end().is_empty() {
                in_body = true;
            }
            continue;
        }
        if line.starts_with("--") {
            break;
        }
        encoded.push_str(line.trim_end());
    }
    if encoded.is_empty() {
        return Err(format!("no base64 block found; captured:\n{}", preview(raw)));
    }
    base64::engine::general_purpose::STANDARD
        .decode(&encoded)
        .map_err(|e| format!("{e}; collected {:?}", preview(&encoded)))
}

fn preview(text: &str) -> String {
    text.chars().take(400).collect()
}

fn holds(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

// ---- the promise about the secret -----------------------------------------------------------------

/// THE CLAIM A READER IS ASKED TO BELIEVE: the mail secret is not in Sard's own file.
///
/// This reads the database OFF DISK — including the write-ahead log, where a value that was written
/// and checkpointed away would still linger — and looks for the secret in the bytes.
#[test]
fn the_setup_round_trips_and_the_secret_never_reaches_the_database() {
    let (conn, dir) = db("roundtrip", BOOK_BYTES);
    let store = RecordingStore::default();

    save(&conn, &config(), SECRET, &store).unwrap();

    assert_eq!(load(&conn).unwrap(), Some(config()), "the reader's setup comes back as it went in");
    assert_eq!(store.peek(SECRET_ACCOUNT).as_deref(), Some(SECRET), "and the secret went to the store");

    for name in ["sard.db", "sard.db-wal", "sard.db-shm"] {
        if let Ok(bytes) = std::fs::read(dir.join(name)) {
            assert!(!holds(&bytes, SECRET.as_bytes()), "{name} carries the secret");
        }
    }
}

/// A machine whose credential store is missing gets a refusal, not a plaintext fallback.
#[test]
fn a_setup_is_refused_when_no_credential_store_exists() {
    let (conn, dir) = db("nostore", BOOK_BYTES);

    let error = save(&conn, &config(), SECRET, &NoStore).unwrap_err();
    assert_eq!(error.code(), "kindle.err.noStore");

    assert_eq!(load(&conn).unwrap(), None, "and nothing half-saved was left behind");
    for name in ["sard.db", "sard.db-wal"] {
        if let Ok(bytes) = std::fs::read(dir.join(name)) {
            assert!(!holds(&bytes, SECRET.as_bytes()), "{name} carries the secret");
        }
    }
}

/// Turning the feature off takes the secret with it.
#[test]
fn forgetting_the_setup_leaves_no_secret_behind() {
    let (conn, _dir) = db("forget", BOOK_BYTES);
    let store = RecordingStore::default();
    save(&conn, &config(), SECRET, &store).unwrap();

    forget(&conn, &store).unwrap();

    assert_eq!(load(&conn).unwrap(), None);
    assert_eq!(store.len(), 0);
}

// ---- what is refused, and when --------------------------------------------------------------------

#[test]
fn an_unusable_setup_is_refused_before_anything_is_stored() {
    let (conn, _dir) = db("invalid", BOOK_BYTES);
    let store = RecordingStore::default();
    let broken = KindleConfig { address: "not-an-address".into(), ..config() };

    let error = save(&conn, &broken, SECRET, &store).unwrap_err();
    assert_eq!(error, KindleError::Invalid("kindle.err.address"));
    assert_eq!(error.code(), "kindle.err.address");

    assert_eq!(load(&conn).unwrap(), None, "nothing was saved");
    assert_eq!(store.len(), 0, "and the secret never reached the store");
}

/// A book too big to attach is refused BEFORE the secret is read and before any connection. The reader
/// should not be asked for a password only to be told the file is too large.
#[test]
fn a_book_over_the_cap_is_refused_before_the_secret_is_read() {
    let big = vec![0u8; (MAX_BOOK_BYTES + 1) as usize];
    let (conn, _dir) = db("toobig", &big);
    let store = RecordingStore::default();
    let mailer = RecordingMailer::default();

    let error = send_book(&conn, BOOK, &config(), &store, &mailer).unwrap_err();
    assert_eq!(error.code(), "kindle.err.tooLarge");
    match error {
        KindleError::TooLarge { bytes, max } => {
            assert_eq!(max, MAX_BOOK_BYTES);
            assert!(bytes > max, "the refusal names the real size: {bytes} vs {max}");
        }
        other => panic!("expected TooLarge, got {other:?}"),
    }
    assert_eq!(store.reads(), 0, "the credential store was never asked");
    assert_eq!(mailer.sent.lock().unwrap().len(), 0, "and nothing was sent");
}

#[test]
fn an_unknown_book_is_not_a_crash() {
    let (conn, _dir) = db("nobook", BOOK_BYTES);
    let store = RecordingStore::default();
    let error = send_book(&conn, "no-such-id", &config(), &store, &RecordingMailer::default()).unwrap_err();
    assert_eq!(error, KindleError::NoBook);
}

// ---- what travels ---------------------------------------------------------------------------------

/// The email carries the book and two addresses. Nothing about the reader's reading.
#[test]
fn the_email_carries_the_book_and_nothing_about_the_reader() {
    let outgoing = Outgoing {
        from: config().from,
        to: config().address,
        subject: "Sard — Book Title".into(),
        body: BODY.into(),
        content_type: "application/epub+zip".into(),
        attachment_name: "Book Title - Author.epub".into(),
        attachment: BOOK_BYTES.to_vec(),
    };

    let raw = String::from_utf8_lossy(&mail::build_message(&config(), &outgoing).unwrap().formatted()).into_owned();

    assert!(raw.contains("To: reader@kindle.com"), "the document goes to the Kindle address");
    assert!(raw.contains("From: reader@example.com"), "and only from an address Amazon has approved");
    assert!(raw.contains("application/epub+zip"), "with its own type");
    assert!(
        raw.contains("Content-Transfer-Encoding: base64"),
        "and an encoding that can carry a binary file, rather than one inferred from the bytes"
    );
    assert!(raw.contains("Sent from Sard."), "and one fixed line of body text");
    assert!(!raw.contains(SECRET), "the secret is not in the message");
    assert!(!raw.contains(BOOK), "nor the book's internal id");
}

/// The name the Kindle library will show comes from the book, not from Sard's managed file — which is
/// named after a hash.
#[test]
fn the_attachment_is_named_from_the_book_not_from_the_managed_file() {
    let managed = Path::new("/library/9f2c1b0a7e.epub");

    assert_eq!(
        attachment_name("مقدمة ابن خلدون", "ابن خلدون", managed),
        "مقدمة ابن خلدون - ابن خلدون.epub",
        "an Arabic title travels, and the label Amazon shows is the book's name"
    );
    assert_eq!(
        attachment_name("A/B: the *title*?", "C", managed),
        "A B the title - C.epub",
        "nothing that a filename or a MIME header cannot hold survives"
    );
    assert_eq!(attachment_name("", "", managed), "book.epub", "a book with no title is still a book");
    assert_eq!(
        attachment_name(&"ط".repeat(400), "", Path::new("/library/x.pdf")),
        format!("{}.pdf", "ط".repeat(120)),
        "cut on a character boundary, so a long Arabic title cannot panic"
    );
}

// ---- the conversation -----------------------------------------------------------------------------

/// THE WHOLE PATH, on a real socket: the message is composed, spoken over SMTP, and the book that
/// arrives at the other end is byte-for-byte the book that left.
#[test]
fn a_book_arrives_at_a_real_smtp_conversation_intact() {
    let (conn, _dir) = db("e2e", BOOK_BYTES);
    let store = RecordingStore::default();
    let server = FakeSmtp::start();

    // The fake server needs no credentials; an empty login means "do not authenticate", which is also
    // a real setup for a reader who runs their own mail server.
    let mut cfg = config();
    cfg.smtp_user = String::new();
    cfg.smtp_host = "127.0.0.1".into();
    cfg.smtp_port = server.port;
    save(&conn, &cfg, SECRET, &store).unwrap();

    let sent = send_book(&conn, BOOK, &cfg, &store, &SmtpMailer::plaintext()).unwrap();

    assert_eq!(sent.attachment, "Book Title - Author.epub");
    assert_eq!(sent.bytes, BOOK_BYTES.len() as u64);

    let raw = server.data();
    assert!(!raw.is_empty(), "the server captured a message");
    let decoded = decode_attachment(&raw).expect("the attachment must decode");
    assert_eq!(decoded, BOOK_BYTES, "the book survived the journey byte for byte");

    let envelope = server.envelope();
    assert!(
        envelope.iter().any(|l| l.to_ascii_uppercase().starts_with("MAIL FROM") && l.contains("reader@example.com")),
        "the envelope sender is the approved address: {envelope:?}"
    );
    assert!(
        envelope.iter().any(|l| l.to_ascii_uppercase().starts_with("RCPT TO") && l.contains("reader@kindle.com")),
        "and the recipient is the reader's Send-to-Kindle address: {envelope:?}"
    );
}
