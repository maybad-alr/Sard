//! The transport, as a seam.
//!
//! Everything above this file is tested without a network, and one test drives the REAL client against
//! a server on localhost. That is only possible because the client is behind a trait: the account
//! backend asks for a request and gets a response, and who answers is the caller's business.

use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Patch,
}

#[derive(Clone, Debug)]
pub struct HttpRequest {
    pub method: Method,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
}

#[derive(Clone, Debug)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// The body as text, for the JSON the account service answers with.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

pub trait Http: Send + Sync {
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse, String>;
}

/// The real client.
///
/// A GLOBAL TIMEOUT, because a sync that hangs is worse than one that fails: the reader's position is
/// already saved locally, and a stalled request would hold the whole pass open behind it.
///
/// NON-2xx IS NOT AN ERROR HERE. The status is data — the account backend reads 401 to know it must
/// sign in again, and 409 to know it lost a race — so it must arrive at the caller as a value rather
/// than as a transport failure that loses the distinction.
pub struct UreqHttp {
    agent: ureq::Agent,
}

impl Default for UreqHttp {
    fn default() -> Self {
        Self::new()
    }
}

impl UreqHttp {
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(30)))
            .http_status_as_error(false)
            .build();
        Self { agent: ureq::Agent::new_with_config(config) }
    }
}

impl Http for UreqHttp {
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse, String> {
        // EACH METHOD IS BUILT AND SENT IN ITS OWN ARM, which looks repetitive and is not: ureq types
        // a request by whether it carries a body (`RequestBuilder<WithBody>` vs `<WithoutBody>`), so a
        // GET and a POST cannot be assigned to one variable. The alternative would be to send every
        // method with an empty body, which would turn a GET into something else on the wire.
        let reply = match request.method {
            Method::Get => {
                let mut builder = self.agent.get(&request.url);
                for (name, value) in &request.headers {
                    builder = builder.header(name.as_str(), value.as_str());
                }
                builder.call()
            }
            Method::Post => {
                let mut builder = self.agent.post(&request.url);
                for (name, value) in &request.headers {
                    builder = builder.header(name.as_str(), value.as_str());
                }
                match &request.body {
                    Some(body) => builder.send(body.as_slice()),
                    None => builder.send_empty(),
                }
            }
            Method::Patch => {
                let mut builder = self.agent.patch(&request.url);
                for (name, value) in &request.headers {
                    builder = builder.header(name.as_str(), value.as_str());
                }
                match &request.body {
                    Some(body) => builder.send(body.as_slice()),
                    None => builder.send_empty(),
                }
            }
        }
        .map_err(|e| e.to_string())?;

        let status = reply.status().as_u16();
        let body = reply.into_body().read_to_vec().map_err(|e| e.to_string())?;
        Ok(HttpResponse { status, body })
    }
}
