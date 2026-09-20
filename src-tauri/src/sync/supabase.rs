//! The account, over HTTP: the identity service for signing in, and the REST layer for the documents.
//!
//! # Why this provider
//!
//! The reader asked for free, correct, and nothing they have to run. This is the one that is all
//! three: a free tier that does not expire for an application this size, and — the part that decides
//! it — row-level security, so the isolation between two readers is enforced by the DATABASE rather
//! than by the application remembering to filter. A backend without that would put every reader's
//! library one forgotten `where` clause away from every other's.
//!
//! # What lives where
//!
//!   · `url` and the publishable key are NOT secrets. The key is designed to be shipped inside a
//!     client, and the security is the policies, not its secrecy. They are ordinary settings rows.
//!   · the REFRESH TOKEN is a secret, and goes to the credential store with every other secret — see
//!     `crate::secrets`.
//!   · the ACCESS TOKEN is never written anywhere. It lives in memory for the length of a pass and is
//!     refreshed when it is close to expiring, or when the service rejects it.
//!
//! # The concurrency contract, and how it is kept up here
//!
//! `store` carries the version the caller read. Against this service that is a conditional update —
//! `version=eq.<n>` — and the service's answer for "no row matched" is an EMPTY ARRAY with a 200, not
//! a status code. A create that collides with an existing row is a 409. Both are turned into
//! `SyncError::Conflict` here, so the merge layer above never learns what a status code is.

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::doc::BookState;
use super::http::{Http, HttpRequest, Method};
use super::{RemoteDoc, SyncBackend, SyncError};
use crate::secrets::SecretStore;

/// The credential-store account the refresh token is filed under.
pub const SECRET_ACCOUNT: &str = "sync.refresh_token";

/// The table the documents live in. Created by `docs/sync-supabase.sql`, which also carries the
/// policies that make one reader's rows invisible to another.
const TABLE: &str = "book_state";

/// How long before expiry a token is refreshed. A pass makes several calls, and a token that expires
/// between two of them would turn a working sync into a sign-in prompt.
const REFRESH_MARGIN_SECS: i64 = 60;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SupabaseConfig {
    /// `https://<project-ref>.supabase.co`
    pub url: String,
    /// The publishable ("anon") key. Public by design.
    pub anon_key: String,
    /// TEST ONLY, and it does not exist in a release build — so a config that arrived from a reader
    /// cannot ask for it, however it is spelled in JSON. It exists because the transport tests run a
    /// server on this machine on purpose; every other caller goes through `check()` with it false.
    #[cfg(test)]
    #[serde(skip)]
    pub allow_local: bool,
}

impl SupabaseConfig {
    /// The setup as it arrives from the interface.
    ///
    /// A constructor rather than a struct literal at the call sites, because the test-only field below
    /// must not be settable from outside this module — not even in a test build, where it exists. The
    /// only code that can turn it on is in this file's own tests.
    pub fn new(url: impl Into<String>, anon_key: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            anon_key: anon_key.into(),
            #[cfg(test)]
            allow_local: false,
        }
    }

    /// MAY A REQUEST BE SENT TO THIS URL? Called before every request, not only when the URL is saved.
    ///
    /// The URL is typed in by the reader and every request carries an access token, which makes it an
    /// input with consequences: one pointing at this machine reaches a service the reader is running,
    /// one pointing at their own network reaches their router or a neighbour's device, and one
    /// pointing at a link-local address reaches a cloud metadata service that hands out credentials.
    /// So the rule is applied at the moment of use — a value that was acceptable when it was saved is
    /// not trusted to still be acceptable now.
    pub fn check(&self) -> Result<(), String> {
        check_host(&self.url, self.local_allowed())
    }

    #[cfg(test)]
    fn local_allowed(&self) -> bool {
        self.allow_local
    }

    #[cfg(not(test))]
    fn local_allowed(&self) -> bool {
        false
    }

    fn base(&self) -> String {
        self.url.trim().trim_end_matches('/').to_string()
    }

    fn auth_url(&self, path: &str) -> String {
        format!("{}/auth/v1/{}", self.base(), path)
    }

    fn rest_url(&self, query: &str) -> String {
        format!("{}/rest/v1/{}", self.base(), query)
    }

    fn json_headers(&self) -> Vec<(String, String)> {
        vec![
            ("apikey".to_string(), self.anon_key.trim().to_string()),
            ("Content-Type".to_string(), "application/json".to_string()),
        ]
    }

    fn bearer_headers(&self, access_token: &str) -> Vec<(String, String)> {
        let mut headers = self.json_headers();
        headers.push(("Authorization".to_string(), format!("Bearer {access_token}")));
        headers
    }
}

/// A signed-in session. Only `refresh_token` is ever persisted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    pub user_id: String,
    pub access_token: String,
    pub refresh_token: String,
    /// Unix seconds.
    pub expires_at: i64,
}

impl Session {
    fn expires_soon(&self, now: i64) -> bool {
        now >= self.expires_at - REFRESH_MARGIN_SECS
    }
}

/// What signing up did. With email confirmation switched on the account is created but there is no
/// session yet, and that is a different sentence to say to the reader than an error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignUp {
    SignedIn(Box<Session>),
    ConfirmEmail,
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// The service's reply on a successful identity call.
#[derive(Deserialize)]
struct TokenReply {
    access_token: String,
    refresh_token: String,
    #[serde(default)]
    expires_at: Option<i64>,
    #[serde(default)]
    expires_in: Option<i64>,
    user: UserRef,
}

#[derive(Deserialize)]
struct UserRef {
    id: String,
}

impl TokenReply {
    fn into_session(self, at: i64) -> Session {
        Session {
            user_id: self.user.id,
            access_token: self.access_token,
            refresh_token: self.refresh_token,
            // `expires_at` is absolute and preferred; `expires_in` is the fallback, and 3600 is the
            // service's own default rather than a guess at it.
            expires_at: self.expires_at.unwrap_or_else(|| at + self.expires_in.unwrap_or(3600)),
        }
    }
}

/// Sign in with an email and a password.
pub fn sign_in(
    http: &dyn Http,
    config: &SupabaseConfig,
    email: &str,
    password: &str,
) -> Result<Session, String> {
    config.check()?;
    let request = HttpRequest {
        method: Method::Post,
        url: config.auth_url("token?grant_type=password"),
        headers: config.json_headers(),
        body: Some(
            serde_json::json!({ "email": email.trim(), "password": password })
                .to_string()
                .into_bytes(),
        ),
    };
    let reply = http.send(&request)?;
    if reply.status != 200 {
        return Err(auth_code(reply.status, &reply.text()));
    }
    let parsed: TokenReply = serde_json::from_str(&reply.text()).map_err(|_| "sync.err.auth".to_string())?;
    Ok(parsed.into_session(now()))
}

/// Create an account.
pub fn sign_up(
    http: &dyn Http,
    config: &SupabaseConfig,
    email: &str,
    password: &str,
) -> Result<SignUp, String> {
    config.check()?;
    let request = HttpRequest {
        method: Method::Post,
        url: config.auth_url("signup"),
        headers: config.json_headers(),
        body: Some(
            serde_json::json!({ "email": email.trim(), "password": password })
                .to_string()
                .into_bytes(),
        ),
    };
    let reply = http.send(&request)?;
    if reply.status != 200 {
        return Err(auth_code(reply.status, &reply.text()));
    }
    // WHICH ANSWER THIS IS DEPENDS ON A PROJECT SETTING, not on the client: with "confirm email" on,
    // the service returns the new user and no tokens at all. Both are success.
    match serde_json::from_str::<TokenReply>(&reply.text()) {
        Ok(tokens) => Ok(SignUp::SignedIn(Box::new(tokens.into_session(now())))),
        Err(_) => Ok(SignUp::ConfirmEmail),
    }
}

/// Rebuild a session from a stored refresh token — what a launch does before its first pass, when the
/// only thing kept from the last one is the key.
pub fn sign_in_with_refresh(
    http: &dyn Http,
    config: &SupabaseConfig,
    refresh_token: &str,
) -> Result<Session, String> {
    refresh(http, config, refresh_token)
}

/// Exchange a refresh token for a fresh session.
fn refresh(http: &dyn Http, config: &SupabaseConfig, refresh_token: &str) -> Result<Session, String> {
    config.check()?;
    let request = HttpRequest {
        method: Method::Post,
        url: config.auth_url("token?grant_type=refresh_token"),
        headers: config.json_headers(),
        body: Some(
            serde_json::json!({ "refresh_token": refresh_token }).to_string().into_bytes(),
        ),
    };
    let reply = http.send(&request)?;
    if reply.status != 200 {
        return Err(auth_code(reply.status, &reply.text()));
    }
    let parsed: TokenReply = serde_json::from_str(&reply.text()).map_err(|_| "sync.err.auth".to_string())?;
    Ok(parsed.into_session(now()))
}

/// A stable key the interface translates, derived from the service's own error code.
fn auth_code(status: u16, body: &str) -> String {
    let code = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("error_code").and_then(|c| c.as_str()).map(str::to_string));
    match (status, code.as_deref()) {
        (400, Some("invalid_credentials")) => "sync.err.badCredentials",
        (400, Some("email_not_confirmed")) => "sync.err.emailNotConfirmed",
        (422, Some("user_already_exists")) => "sync.err.userExists",
        (422, Some("weak_password")) => "sync.err.weakPassword",
        (429, _) => "sync.err.tooManyTries",
        (401, _) => "sync.err.sessionExpired",
        _ => "sync.err.auth",
    }
    .to_string()
}

/// One row of the account's table, as the service returns it. The book id is deliberately NOT a field:
/// the caller asked for one book and knows its id, and a field nothing reads is a field that will
/// disagree with the query one day. serde ignores it on the way in.
#[derive(Deserialize)]
struct Row {
    version: i64,
    doc: BookState,
}

/// The account's storage, over this service.
pub struct SupabaseBackend<'a> {
    config: SupabaseConfig,
    http: &'a dyn Http,
    secrets: &'a dyn SecretStore,
    session: Mutex<Session>,
}

impl<'a> SupabaseBackend<'a> {
    pub fn new(
        config: SupabaseConfig,
        http: &'a dyn Http,
        secrets: &'a dyn SecretStore,
        session: Session,
    ) -> Self {
        Self { config, http, secrets, session: Mutex::new(session) }
    }

    /// Exchange the stored refresh token for a new session, and keep the new one.
    ///
    /// The NEW refresh token is written back immediately: the service may rotate it, and a token that
    /// was replaced but not saved is a session the reader loses at the next launch.
    fn reauthorize(&self) -> Result<(), SyncError> {
        let stored = self
            .secrets
            .secret(SECRET_ACCOUNT)
            .map_err(SyncError::Transport)?
            .ok_or_else(|| SyncError::Transport("sync.err.sessionExpired".into()))?;
        let fresh = refresh(self.http, &self.config, &stored).map_err(SyncError::Transport)?;
        self.secrets
            .put(SECRET_ACCOUNT, &fresh.refresh_token)
            .map_err(SyncError::Transport)?;
        *self.session.lock().unwrap() = fresh;
        Ok(())
    }

    /// A usable access token: refreshed first when it is close to expiring.
    fn access_token(&self) -> Result<String, SyncError> {
        let needs = { self.session.lock().unwrap().expires_soon(now()) };
        if needs {
            self.reauthorize()?;
        }
        Ok(self.session.lock().unwrap().access_token.clone())
    }

    fn user_id(&self) -> String {
        self.session.lock().unwrap().user_id.clone()
    }

    /// Send an authenticated request, signing in again ONCE if the service rejects the token.
    ///
    /// The retry is what makes a long-idle application work: the access token is short-lived by design,
    /// and a pass that happened to start with a stale one should not become a sign-in prompt.
    fn send_authed(&self, build: impl Fn(&str) -> HttpRequest) -> Result<super::http::HttpResponse, SyncError> {
        // The rule is applied HERE as well as where the URL is saved: it is the last point before a
        // request leaves with an access token attached.
        self.config.check().map_err(SyncError::Transport)?;
        let token = self.access_token()?;
        let mut reply = self.http.send(&build(&token)).map_err(SyncError::Transport)?;
        if reply.status == 401 {
            self.reauthorize()?;
            let token = self.access_token()?;
            reply = self.http.send(&build(&token)).map_err(SyncError::Transport)?;
        }
        Ok(reply)
    }

    /// The row filters. `book_id` is a hex digest and `user_id` is a uuid — neither needs escaping,
    /// which is why these are built by hand rather than through an encoder.
    fn own_rows(&self, extra: &str) -> String {
        format!("{TABLE}?user_id=eq.{}{extra}", self.user_id())
    }
}

impl SyncBackend for SupabaseBackend<'_> {
    fn fetch(&self, book_id: &str) -> Result<Option<RemoteDoc>, SyncError> {
        let reply = self.send_authed(|token| HttpRequest {
            method: Method::Get,
            url: self.config.rest_url(&self.own_rows(&format!("&book_id=eq.{book_id}&select=book_id,version,doc"))),
            headers: self.config.bearer_headers(token),
            body: None,
        })?;
        if reply.status != 200 {
            return Err(SyncError::Transport(http_code(reply.status)));
        }
        let rows: Vec<Row> = serde_json::from_str(&reply.text()).map_err(|_| SyncError::Transport("sync.err.unreadable".into()))?;
        Ok(rows.into_iter().next().map(|row| RemoteDoc {
            doc: row.doc,
            version: row.version.max(0) as u64,
        }))
    }

    fn store(&self, book_id: &str, doc: &BookState, expected: Option<u64>) -> Result<u64, SyncError> {
        match expected {
            // An update that only applies if the account still holds the version we read. The
            // service's answer for "nothing matched" is an empty array with a 200 — not an error
            // status — so the BODY is what says whether this won or lost.
            Some(version) => {
                let next = version + 1;
                let reply = self.send_authed(|token| HttpRequest {
                    method: Method::Patch,
                    url: self.config.rest_url(&self.own_rows(&format!("&book_id=eq.{book_id}&version=eq.{version}"))),
                    headers: self.config.bearer_headers(token).into_iter().chain([(
                        "Prefer".to_string(),
                        "return=representation".to_string(),
                    )]).collect(),
                    body: Some(
                        serde_json::json!({ "doc": doc, "version": next as i64, "updated_at": now() })
                            .to_string()
                            .into_bytes(),
                    ),
                })?;
                if reply.status != 200 {
                    return Err(SyncError::Transport(http_code(reply.status)));
                }
                let written: Vec<serde_json::Value> =
                    serde_json::from_str(&reply.text()).map_err(|_| SyncError::Transport("sync.err.unreadable".into()))?;
                if written.is_empty() {
                    return Err(SyncError::Conflict);
                }
                Ok(next)
            }
            // A create. A collision is a 409 from the primary key, which means another device created
            // this book while we were fetching — the same race as above, expressed by a status code.
            None => {
                let user_id = self.user_id();
                let reply = self.send_authed(|token| HttpRequest {
                    method: Method::Post,
                    url: self.config.rest_url(TABLE),
                    headers: self.config.bearer_headers(token).into_iter().chain([(
                        "Prefer".to_string(),
                        "return=representation".to_string(),
                    )]).collect(),
                    body: Some(
                        serde_json::json!({
                            "user_id": user_id,
                            "book_id": book_id,
                            "version": 1,
                            "doc": doc,
                        })
                        .to_string()
                        .into_bytes(),
                    ),
                })?;
                match reply.status {
                    201 => Ok(1),
                    409 => Err(SyncError::Conflict),
                    status => Err(SyncError::Transport(http_code(status))),
                }
            }
        }
    }

    fn versions(&self) -> Result<Vec<(String, u64)>, SyncError> {
        #[derive(Deserialize)]
        struct VersionRow {
            book_id: String,
            version: i64,
        }

        let reply = self.send_authed(|token| HttpRequest {
            method: Method::Get,
            url: self.config.rest_url(&self.own_rows("&select=book_id,version")),
            headers: self.config.bearer_headers(token),
            body: None,
        })?;
        if reply.status != 200 {
            return Err(SyncError::Transport(http_code(reply.status)));
        }
        let rows: Vec<VersionRow> =
            serde_json::from_str(&reply.text()).map_err(|_| SyncError::Transport("sync.err.unreadable".into()))?;
        Ok(rows.into_iter().map(|r| (r.book_id, r.version.max(0) as u64)).collect())
    }
}

fn http_code(status: u16) -> String {
    match status {
        401 | 403 => "sync.err.sessionExpired".to_string(),
        404 => "sync.err.noTable".to_string(),
        other => format!("sync.err.http{other}"),
    }
}

/// The URL rule, as a function of the string so it can be tested on its own.
///
/// WHAT IT CANNOT DO, said plainly rather than implied: it judges the URL as WRITTEN. A hostname that
/// resolves to a private address at request time would pass, and catching that needs a check at
/// connection time, which the HTTP client does not offer. What it does catch is every literal address
/// and every shape that puts a request inside the reader's own network or machine — which is the class
/// that a pasted value can reach without anyone meaning it to.
pub(crate) fn check_host(url: &str, allow_local: bool) -> Result<(), String> {
    let trimmed = url.trim();
    let lowered = trimmed.to_ascii_lowercase();
    if !(lowered.starts_with("https://") || lowered.starts_with("http://")) {
        return Err("sync.err.urlScheme".into());
    }
    let authority = trimmed
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or("")
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("");
    if authority.is_empty() {
        return Err("sync.err.urlShape".into());
    }
    // `user@host` is a shape that exists to be misread, and no project URL needs it: the address part
    // before the @ is what a human reads, while the part after it is what would be contacted.
    if authority.contains('@') {
        return Err("sync.err.urlShape".into());
    }

    let host = match authority.strip_prefix('[') {
        // An IPv6 literal: the address is inside the brackets, any port is after them.
        Some(rest) => rest.split(']').next().unwrap_or("").to_string(),
        None => authority.split(':').next().unwrap_or("").to_string(),
    };
    // A trailing dot is the same name, spelled differently — `localhost.` resolves like `localhost`.
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        return Err("sync.err.urlShape".into());
    }

    if host == "localhost" || host.ends_with(".localhost") {
        return if allow_local { Ok(()) } else { Err("sync.err.urlLocal".into()) };
    }

    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        let loopback = match ip {
            std::net::IpAddr::V4(v4) => v4.is_loopback(),
            std::net::IpAddr::V6(v6) => v6.is_loopback(),
        };
        if loopback && allow_local {
            return Ok(());
        }
        if is_local_or_reserved(ip) {
            return Err("sync.err.urlLocal".into());
        }
    }
    Ok(())
}

/// Is this address inside the reader's own machine, their own network, or somewhere reserved?
fn is_local_or_reserved(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let octets = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local() // 169.254/16 — the cloud metadata range
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_documentation()
                || octets[0] >= 240 // reserved, and the old class E
                || (octets[0] == 100 && (64..=127).contains(&octets[1])) // carrier-grade NAT
                || (octets[0] == 198 && (octets[1] == 18 || octets[1] == 19)) // benchmarking
        }
        std::net::IpAddr::V6(v6) => {
            if v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() {
                return true;
            }
            let segments = v6.segments();
            let link_local = segments[0] & 0xffc0 == 0xfe80;
            let unique_local = segments[0] & 0xfe00 == 0xfc00;
            if link_local || unique_local {
                return true;
            }
            // A v4-mapped address is a v4 address wearing a v6 hat.
            match v6.to_ipv4_mapped() {
                Some(v4) => is_local_or_reserved(std::net::IpAddr::V4(v4)),
                None => false,
            }
        }
    }
}
