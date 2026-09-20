//! The message, and the conversation that carries it.
//!
//! Split from `kindle` on purpose: everything above this file decides WHETHER a book should travel and
//! what may be said about it; this file only knows how to put bytes on the wire. The `Mailer` seam is
//! what lets the rules above be tested without a network, and what lets one test drive a REAL SMTP
//! conversation against a server that exists only for that test.

use std::time::Duration;

use lettre::message::header::{ContentDisposition, ContentTransferEncoding, ContentType};
use lettre::message::{Mailbox, Message, MultiPart, SinglePart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::SmtpTransport;
use lettre::Transport;

use super::KindleConfig;

/// One book, ready to travel. Bytes in, bytes out — this type carries no reading data of any kind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outgoing {
    pub from: String,
    pub to: String,
    pub subject: String,
    pub body: String,
    pub content_type: String,
    pub attachment_name: String,
    pub attachment: Vec<u8>,
}

/// Sending, as a seam. The secret is passed IN rather than read here, so this layer never touches the
/// credential store — and so a test can prove the secret is nowhere in what is sent.
pub trait Mailer {
    fn send(&self, config: &KindleConfig, secret: &str, outgoing: &Outgoing) -> Result<(), String>;
}

/// The real one.
///
/// STARTTLS ONLY, DELIBERATELY. Submission over port 587 with an upgrade to TLS is what every provider
/// Sard is likely to meet supports, and it is the shape that cannot silently send a secret in the
/// clear: the credentials are only offered after the upgrade. The implicit-TLS port (465) and plain
/// SMTP are left out rather than half-supported — a transport mode that decides whether a secret is
/// encrypted should not be a field a reader can set by hand.
pub struct SmtpMailer {
    /// Test-only: a plaintext server on localhost. Never reachable from the product, because the only
    /// constructor for it is behind `cfg(test)`.
    plaintext: bool,
}

impl SmtpMailer {
    pub fn starttls() -> Self {
        Self { plaintext: false }
    }

    #[cfg(test)]
    pub fn plaintext() -> Self {
        Self { plaintext: true }
    }
}

impl Mailer for SmtpMailer {
    fn send(&self, config: &KindleConfig, secret: &str, outgoing: &Outgoing) -> Result<(), String> {
        let message = build_message(config, outgoing)?;

        let mut builder = if self.plaintext {
            SmtpTransport::builder_dangerous(config.smtp_host.trim())
        } else {
            SmtpTransport::starttls_relay(config.smtp_host.trim()).map_err(|e| e.to_string())?
        };
        // A ceiling on the whole exchange: a send that hangs must fail rather than leave the interface
        // waiting on a socket forever.
        builder = builder.port(config.smtp_port).timeout(Some(Duration::from_secs(45)));

        // AN EMPTY LOGIN MEANS "DO NOT AUTHENTICATE", which is a legitimate setup — a mail server the
        // reader runs themselves often wants no credentials. It is not an error state, and refusing it
        // would make the feature unusable for exactly the readers who care most about where mail goes.
        let user = config.smtp_user.trim();
        if !user.is_empty() && !secret.is_empty() {
            builder = builder.credentials(Credentials::new(user.to_string(), secret.to_string()));
        }

        builder.build().send(&message).map(|_| ()).map_err(|e| e.to_string())
    }
}

/// Compose the document email.
///
/// The body is one fixed line and the attachment is the reader's file: the message says nothing about
/// what has been read, where the reader stopped, or what else is in the library. Amazon needs a file.
pub fn build_message(config: &KindleConfig, outgoing: &Outgoing) -> Result<Message, String> {
    let from = config.from.trim().parse().map_err(|_| "kindle.err.from".to_string())?;
    let to = outgoing.to.trim().parse().map_err(|_| "kindle.err.address".to_string())?;
    let content_type = ContentType::parse(&outgoing.content_type).map_err(|e| e.to_string())?;

    Message::builder()
        .from(Mailbox::new(None, from))
        .to(Mailbox::new(None, to))
        .subject(outgoing.subject.clone())
        .multipart(
            MultiPart::mixed()
                .singlepart(SinglePart::plain(outgoing.body.clone()))
                // THE ATTACHMENT'S ENCODING IS PINNED, NOT LEFT TO BE INFERRED.
                //
                // This was measured, not assumed: with the encoding left to the library's own choice,
                // an ASCII payload went out declared `7bit` — and a real book is a zip, full of bytes
                // above 127, CR/LF on their own, and NULs. A part declared 7bit while carrying those
                // is mangled by the first hop that normalises line endings, and it arrives truncated
                // rather than refused. base64 is what makes an arbitrary file survive SMTP, so it is
                // stated here rather than hoped for.
                .singlepart(
                    SinglePart::builder()
                        .header(content_type)
                        .header(ContentDisposition::attachment(&outgoing.attachment_name))
                        .header(ContentTransferEncoding::Base64)
                        .body(outgoing.attachment.clone()),
                ),
        )
        .map_err(|e| e.to_string())
}
