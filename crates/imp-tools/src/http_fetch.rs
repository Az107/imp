//! `http_fetch`: one guarded outbound request.
//!
//! This tool is the SSRF boundary for the agent (SDD §5.5, threat T3). Four
//! checks stand between a model-supplied URL and a socket:
//!
//! 1. **Scheme and host.** The host must match `[http_fetch].allowed_domains`,
//!    and `https` is required unless the host is *named* by an exact entry —
//!    `http` is the exception a local dev server needs, and it is granted only
//!    by naming the host, never by a wildcard (SDD §5.5, §11.1).
//! 2. **Address.** With `block_private_ips` on, every address the host resolves
//!    to is rejected when it is private, loopback, link-local or otherwise
//!    non-public, so `169.254.169.254` and the rest of the metadata surface
//!    never answer.
//! 3. **Redirects.** Hops are followed one at a time and every hop is re-checked,
//!    so a redirect cannot carry the request to a host the first check refused.
//! 4. **Size and time.** The body is read up to a cap and the exchange is
//!    bounded by a timeout.
//!
//! Approval is a *separate* mechanism, and the two are deliberately not the
//! same list. `allowed_domains` says what is reachable at all; the ordinary
//! `[policy.allow]` rules say what is pre-approved, matched against the host
//! because [`HttpFetch::approval_subject`] returns it. The tool is
//! `Risk::Network`, so surprise and default both fail closed: nothing is
//! fetchable until a domain is added, and a non-TTY still refuses the call
//! unless it was allowlisted or `--yes` was passed (D15).

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Url;
use reqwest::header::{HeaderName, HeaderValue, LOCATION};
use schemars::JsonSchema;
use serde::Deserialize;

use imp_core::config::HttpFetchConfig;
use imp_core::error::{Error, Result};
use imp_core::glob::glob_match;
use imp_core::tool::{Risk, Tool, ToolCtx, ToolOutput};

/// Redirects followed before the request fails.
const MAX_REDIRECTS: usize = 5;
/// Headers the model may not set, because the client owns them.
const RESERVED_HEADERS: [&str; 4] = ["host", "content-length", "connection", "transfer-encoding"];
/// Slack added to the configured budget so the outer timeout never fires first.
const TIMEOUT_SLACK: Duration = Duration::from_secs(5);

/// HTTP method. Restricted on purpose: a fetch tool does not need `TRACE` or
/// `CONNECT`, and a shorter list is a shorter schema for the model to get wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "UPPERCASE")]
pub enum Method {
    /// Read a resource.
    Get,
    /// Send a body and read the result.
    Post,
    /// Replace a resource.
    Put,
    /// Remove a resource.
    Delete,
}

impl Method {
    fn as_reqwest(self) -> reqwest::Method {
        match self {
            Method::Get => reqwest::Method::GET,
            Method::Post => reqwest::Method::POST,
            Method::Put => reqwest::Method::PUT,
            Method::Delete => reqwest::Method::DELETE,
        }
    }

    /// The method a redirect leaves behind.
    ///
    /// 301/302/303 turn a non-GET into a GET and drop the body, as browsers do;
    /// 307/308 preserve both. Getting this wrong re-sends a POST body to a new
    /// location, which is the classic redirect footgun.
    fn after_redirect(self, status: u16) -> Self {
        match status {
            301..=303 if self != Method::Get => Method::Get,
            _ => self,
        }
    }
}

/// Arguments accepted by [`HttpFetch`].
#[derive(Debug, Deserialize, JsonSchema)]
pub struct HttpFetchArgs {
    /// Absolute URL. `https` unless the host is in `allowed_domains`.
    pub url: String,
    /// HTTP method; defaults to `GET`.
    pub method: Option<Method>,
    /// Extra request headers.
    pub headers: Option<BTreeMap<String, String>>,
    /// Request body, for `POST` and `PUT`.
    pub body: Option<String>,
    /// Cap on the response body. Clamped to the configured maximum.
    pub max_bytes: Option<u64>,
}

/// Fetches a URL, subject to the SSRF guard.
pub struct HttpFetch {
    /// Hosts that may be reached, as exact names or globs.
    allowed_domains: Vec<String>,
    /// Whether a non-public resolved address is refused.
    block_private_ips: bool,
    /// Ceiling on any response body.
    max_bytes: u64,
    /// Wall-clock budget for one request.
    timeout: Duration,
    /// The outbound client. Redirects are handled by hand so every hop can be
    /// checked, which is why it is built with `Policy::none()`.
    client: reqwest::Client,
}

impl HttpFetch {
    /// Build from the `[http_fetch]` config section.
    pub fn new(config: &HttpFetchConfig) -> Self {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(config.timeout_secs.max(1)))
            .user_agent(concat!("imp/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("reqwest client construction cannot fail with these settings");
        Self {
            allowed_domains: config.allowed_domains.clone(),
            block_private_ips: config.block_private_ips,
            max_bytes: config.max_bytes,
            timeout: Duration::from_secs(config.timeout_secs.max(1)),
            client,
        }
    }

    /// Whether `host` matches one of the allowlist entries.
    ///
    /// Matching is the anchored glob from `imp-core`, lowercased on both
    /// sides. An entry without a wildcard is exact, so `example.com` does not
    /// admit `evil.example.com`; `*.example.com` does.
    fn host_allowed(&self, host: &str) -> bool {
        let host = normalize_host(host);
        self.allowed_domains.iter().any(|entry| {
            let pattern = entry.trim().to_ascii_lowercase();
            !pattern.is_empty() && glob_match(&pattern, &host)
        })
    }

    /// Whether `host` is *named* by an entry, with no wildcard.
    ///
    /// This is the stricter test the scheme check asks for: a glob says a host
    /// is reachable, it does not name one. `http` is granted only to a host an
    /// exact entry names (SDD §5.5, §11.1), so `local*` admits `https` for
    /// `localhost` but never plain `http`.
    fn host_named_exactly(&self, host: &str) -> bool {
        let host = normalize_host(host);
        self.allowed_domains
            .iter()
            .any(|entry| !entry.contains('*') && entry.trim().to_ascii_lowercase() == host)
    }

    /// Scheme and host, without touching the network.
    fn check_scheme_and_host(&self, url: &Url) -> Result<()> {
        let host = url
            .host_str()
            .ok_or_else(|| Error::Denied(format!("`{url}` has no host")))?;
        if !self.host_allowed(host) {
            return Err(Error::Denied(format!(
                "host `{host}` is not in [http_fetch].allowed_domains"
            )));
        }
        match url.scheme() {
            // https is the default and the only scheme a wildcard can grant.
            "https" => Ok(()),
            // Plain http is the local-dev-server exception, and it is granted
            // only by *naming* the host: a glob widens reachability, not the
            // scheme. SDD §5.5 and §11.1 both say "explicitly allowlisted".
            "http" if self.host_named_exactly(host) => Ok(()),
            "http" => Err(Error::Denied(format!(
                "scheme `http` needs `{host}` named exactly in [http_fetch].allowed_domains; \
                 a wildcard entry only grants https"
            ))),
            other => Err(Error::Denied(format!(
                "scheme `{other}` is not allowed; use https (or http for an allowlisted host)"
            ))),
        }
    }

    /// Reject any address the host resolves to that is not on the public
    /// internet.
    async fn check_addresses(&self, url: &Url) -> Result<()> {
        if !self.block_private_ips {
            return Ok(());
        }
        let host = url.host_str().unwrap_or_default();
        if let Some(ip) = literal_ip(host) {
            return refuse_if_blocked(ip);
        }
        let port = url.port_or_known_default().unwrap_or(443);
        let addresses = tokio::net::lookup_host((host, port))
            .await
            .map_err(|err| Error::Tool {
                tool: "http_fetch".to_string(),
                message: format!("cannot resolve `{host}`: {err}"),
            })?;
        let mut resolved = false;
        for address in addresses {
            resolved = true;
            refuse_if_blocked(address.ip())?;
        }
        if !resolved {
            return Err(Error::Tool {
                tool: "http_fetch".to_string(),
                message: format!("`{host}` resolved to no addresses"),
            });
        }
        Ok(())
    }

    /// The full guard for one URL.
    async fn guard(&self, url: &Url) -> Result<()> {
        self.check_scheme_and_host(url)?;
        self.check_addresses(url).await
    }

    /// Build the request for one hop.
    fn request(
        &self,
        url: &Url,
        method: Method,
        headers: &[(String, String)],
        body: Option<&str>,
    ) -> reqwest::RequestBuilder {
        let mut request = self.client.request(method.as_reqwest(), url.clone());
        for (name, value) in headers {
            request = request.header(name.as_str(), value.as_str());
        }
        if let Some(body) = body {
            request = request.body(body.to_string());
        }
        request
    }
}

#[async_trait]
impl Tool for HttpFetch {
    fn name(&self) -> &'static str {
        "http_fetch"
    }

    fn description(&self) -> &'static str {
        "Fetch a URL over the network and return its status, headers and body. \
         Only hosts in the allowed-domain list are reachable, private and \
         metadata addresses are refused, and redirects are not followed off an \
         allowed host. Needs approval unless allowlisted."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(HttpFetchArgs)).unwrap_or_default()
    }

    fn risk(&self) -> Risk {
        // The request leaves the machine and can be induced by untrusted
        // content in a transcript, so it is gated like a write (D15).
        Risk::Network
    }

    fn approval_subject(&self, args: &serde_json::Value) -> Option<String> {
        // Match on the host, so an allow rule names the domain rather than
        // blessing every `http_fetch` in the session.
        let url = Url::parse(args.get("url")?.as_str()?).ok()?;
        url.host_str().map(|host| host.to_ascii_lowercase())
    }

    fn timeout(&self) -> Duration {
        // The client enforces the real budget; this is only a backstop, so it is
        // deliberately looser than the configured timeout.
        self.timeout + TIMEOUT_SLACK
    }

    async fn invoke(&self, ctx: ToolCtx, args: serde_json::Value) -> Result<ToolOutput> {
        let args: HttpFetchArgs = serde_json::from_value(args).map_err(|err| Error::ToolArgs {
            tool: self.name().to_string(),
            message: err.to_string(),
        })?;

        let mut url = Url::parse(args.url.trim()).map_err(|err| Error::ToolArgs {
            tool: self.name().to_string(),
            message: format!("invalid URL `{}`: {err}", args.url.trim()),
        })?;

        let cap = args
            .max_bytes
            .unwrap_or(self.max_bytes)
            .clamp(1, self.max_bytes.max(1)) as usize;
        let mut method = args.method.unwrap_or(Method::Get);
        let mut body = args.body;
        let headers = sanitize_headers(args.headers);

        let mut redirects = 0usize;
        loop {
            if ctx.cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }

            // Every hop is checked, the first included: a redirect is a new
            // request, and a cross-domain one lands here as a refusal.
            self.guard(&url)
                .await
                .map_err(|err| match (redirects, err) {
                    (0, err) => err,
                    // Strip the inner prefix so the message reads as one sentence.
                    (_, Error::Denied(reason)) => {
                        Error::Denied(format!("redirect refused: {reason}"))
                    }
                    (_, err) => err,
                })?;

            let mut response = self
                .request(&url, method, &headers, body.as_deref())
                .send()
                .await
                .map_err(|err| Error::Tool {
                    tool: self.name().to_string(),
                    message: format!("request to `{url}` failed: {err}"),
                })?;
            let status = response.status();

            if status.is_redirection()
                && let Some(location) = response.headers().get(LOCATION)
            {
                if redirects >= MAX_REDIRECTS {
                    return Err(Error::Tool {
                        tool: self.name().to_string(),
                        message: format!("more than {MAX_REDIRECTS} redirects from `{url}`"),
                    });
                }
                let location = location.to_str().map_err(|_| Error::Tool {
                    tool: self.name().to_string(),
                    message: "the redirect location is not valid text".to_string(),
                })?;
                let next = url.join(location).map_err(|err| Error::Tool {
                    tool: self.name().to_string(),
                    message: format!("cannot resolve redirect `{location}`: {err}"),
                })?;
                method = method.after_redirect(status.as_u16());
                if method == Method::Get {
                    body = None;
                }
                redirects += 1;
                url = next;
                continue;
            }

            let (body_text, truncated) = read_capped(&mut response, cap).await?;
            let header_map = collect_headers(&response);

            let payload = serde_json::json!({
                "status": status.as_u16(),
                "url": url.as_str(),
                "redirects": redirects,
                "headers": header_map,
                "body": body_text,
                "truncated": truncated,
            });

            return Ok(ToolOutput {
                content: payload.to_string(),
                truncated,
                metadata: serde_json::json!({
                    "status": status.as_u16(),
                    "url": url.as_str(),
                    "redirects": redirects,
                    "kept_bytes": body_text.len(),
                    "truncated": truncated,
                }),
            });
        }
    }
}

/// Read the body, keeping at most `cap` bytes.
///
/// Unlike `run_command`, stopping early is safe here: the response is dropped
/// and its connection closed. The timeout bounds a server that would otherwise
/// stream forever.
async fn read_capped(response: &mut reqwest::Response, cap: usize) -> Result<(String, bool)> {
    let mut kept: Vec<u8> = Vec::new();
    let mut truncated = false;
    loop {
        let chunk = response.chunk().await.map_err(|err| Error::Tool {
            tool: "http_fetch".to_string(),
            message: format!("cannot read the response body: {err}"),
        })?;
        let Some(chunk) = chunk else { break };
        let room = cap.saturating_sub(kept.len());
        if chunk.len() <= room {
            kept.extend_from_slice(&chunk);
        } else {
            kept.extend_from_slice(&chunk[..room]);
            truncated = true;
            break;
        }
    }
    // A body that is not UTF-8 is still worth showing; the model reads text, and
    // a replacement character is a clearer signal than an error. `metadata`
    // carries the kept size so a caller can tell truncation from corruption.
    Ok((String::from_utf8_lossy(&kept).into_owned(), truncated))
}

/// Response headers flattened to one `name -> value` map.
fn collect_headers(response: &reqwest::Response) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    for (name, value) in response.headers() {
        let value = value.to_str().unwrap_or("<binary>");
        out.entry(name.as_str().to_string())
            .and_modify(|existing| {
                existing.push_str(", ");
                existing.push_str(value);
            })
            .or_insert_with(|| value.to_string());
    }
    out
}

/// Model-supplied headers, minus anything invalid or client-owned.
fn sanitize_headers(headers: Option<BTreeMap<String, String>>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (name, value) in headers.unwrap_or_default() {
        if RESERVED_HEADERS.contains(&name.to_ascii_lowercase().as_str()) {
            continue;
        }
        if HeaderName::from_bytes(name.as_bytes()).is_err()
            || HeaderValue::from_str(&value).is_err()
        {
            continue;
        }
        out.push((name, value));
    }
    out
}

/// Canonical form of a URL host for matching.
///
/// Lowercased, and IPv6 brackets removed, so `[::1]` and `::1` are the same
/// host. `Url::host_str` hands back an IPv6 host with its brackets, while a
/// user writing an allowlist entry almost always writes the bare form; folding
/// them here closes that gap. Both `host_allowed` and `host_named_exactly` go
/// through this, so the two agree on what a host is.
fn normalize_host(host: &str) -> String {
    host.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase()
}

/// The address in a URL host, if it is an IP literal rather than a name.
///
/// `Url::host_str` hands back IPv6 without the brackets, but accepting both
/// costs nothing and removes a way to get this wrong.
fn literal_ip(host: &str) -> Option<IpAddr> {
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .parse()
        .ok()
}

/// Refuse an address that is not publicly routable.
fn refuse_if_blocked(ip: IpAddr) -> Result<()> {
    if is_blocked_address(ip) {
        return Err(Error::Denied(format!(
            "address `{ip}` is private, loopback, link-local or otherwise non-public; \
             refusing to connect"
        )));
    }
    Ok(())
}

/// Whether an address must never be reached by a model-supplied URL.
///
/// Broader than "private", on purpose: the ranges below are exactly the ones a
/// cloud metadata service, a local control plane, or the host's own loopback
/// live on, and none of them is a place an agent fetch has any business going.
fn is_blocked_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, ..] = ip.octets();
            ip.is_private()          // 10/8, 172.16/12, 192.168/16
                || ip.is_loopback()  // 127/8
                || ip.is_link_local() // 169.254/16, incl. 169.254.169.254
                || ip.is_broadcast()
                || ip.is_documentation()
                || ip.is_multicast()
                || a == 0                              // 0.0.0.0/8 "this network"
                || (a == 100 && (64..=127).contains(&b)) // 100.64/10 CGNAT
                || (a == 192 && b == 0)                // 192.0.0/24 IETF assignments
                || (a == 198 && (b == 18 || b == 19)) // 198.18/15 benchmarking
        }
        IpAddr::V6(ip) => {
            let first = ip.segments()[0];
            ip.is_loopback()
                || ip.is_multicast()
                // ::/128 unspecified, and every other IPv6 form that embeds a
                // v4 address (e.g. ::ffff:169.254.169.254) is judged by the v4.
                || ip.to_ipv4_mapped().map(IpAddr::V4).map(is_blocked_address).unwrap_or(false)
                || ip.to_ipv4().map(IpAddr::V4).map(is_blocked_address).unwrap_or(false)
                || first == 0
                || (first & 0xfe00) == 0xfc00 // fc00::/7 unique local
                || (first & 0xffc0) == 0xfe80 // fe80::/10 link local
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_util::sync::CancellationToken;

    fn fetcher(allowed: &[&str], block_private_ips: bool) -> HttpFetch {
        HttpFetch::new(&HttpFetchConfig {
            allowed_domains: allowed.iter().map(|entry| entry.to_string()).collect(),
            block_private_ips,
            max_bytes: 1 << 20,
            timeout_secs: 5,
        })
    }

    fn ctx() -> ToolCtx {
        ToolCtx {
            workspace_root: PathBuf::from("/tmp"),
            cancel: CancellationToken::new(),
        }
    }

    async fn fetch(tool: &HttpFetch, url: &str) -> Result<ToolOutput> {
        tool.invoke(ctx(), serde_json::json!({ "url": url })).await
    }

    fn ok_response(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn redirect_response(location: &str) -> String {
        format!(
            "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\n\
             Connection: close\r\n\r\n"
        )
    }

    /// Serve `responses`, one per accepted connection, then stop. Returns the
    /// base URL. Binds port 0 so tests never collide.
    async fn spawn_server(responses: Vec<String>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            for response in responses {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let mut scratch = [0u8; 2048];
                let _ = socket.read(&mut scratch).await;
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        format!("http://{address}")
    }

    // --------------------------------------------------------------- guard

    #[tokio::test]
    async fn a_loopback_url_is_refused_even_when_it_is_allowlisted() {
        // The domain allowlist says "reachable"; the address guard says "not
        // this address". The address guard wins, which is the whole point of
        // having two lists.
        let tool = fetcher(&["127.0.0.1"], true);

        let err = fetch(&tool, "http://127.0.0.1:1/secret")
            .await
            .expect_err("loopback must be refused");

        assert!(matches!(err, Error::Denied(_)), "was: {err}");
        assert!(err.to_string().contains("non-public"), "was: {err}");
    }

    #[tokio::test]
    async fn the_cloud_metadata_address_is_refused() {
        let tool = fetcher(&["169.254.169.254"], true);

        let err = fetch(&tool, "http://169.254.169.254/latest/meta-data/")
            .await
            .expect_err("metadata must be refused");

        assert!(matches!(err, Error::Denied(_)), "was: {err}");
    }

    #[test]
    fn the_address_classifier_covers_the_usual_ssrf_targets() {
        for blocked in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.0.1",
            "169.254.169.254",
            "0.0.0.0",
            "100.64.0.1",
            "::1",
            "fe80::1",
            "fc00::1",
            "::ffff:127.0.0.1",
        ] {
            let ip: IpAddr = blocked.parse().unwrap();
            assert!(is_blocked_address(ip), "{blocked} should be refused");
        }

        for public in ["93.184.216.34", "8.8.8.8", "2606:4700:4700::1111"] {
            let ip: IpAddr = public.parse().unwrap();
            assert!(!is_blocked_address(ip), "{public} should be allowed");
        }
    }

    #[test]
    fn the_allowlist_is_exact_unless_it_wildcards() {
        let tool = fetcher(&["example.com", "*.github.com"], true);

        assert!(tool.host_allowed("example.com"));
        assert!(
            tool.host_allowed("EXAMPLE.com"),
            "hosts are case-insensitive"
        );
        assert!(tool.host_allowed("api.github.com"));
        assert!(
            !tool.host_allowed("evil.example.com"),
            "an exact entry must not admit a subdomain"
        );
        assert!(!tool.host_allowed("github.com"));
        assert!(!tool.host_allowed("example.com.evil.test"));
    }

    #[test]
    fn an_empty_allowlist_reaches_nothing() {
        let tool = fetcher(&[], true);
        assert!(!tool.host_allowed("example.com"));
    }

    #[tokio::test]
    async fn a_host_outside_the_allowlist_is_refused() {
        let tool = fetcher(&["docs.rs"], true);

        let err = fetch(&tool, "https://example.com/")
            .await
            .expect_err("a host outside the allowlist must be refused");

        assert!(matches!(err, Error::Denied(_)), "was: {err}");
        assert!(err.to_string().contains("allowed_domains"), "was: {err}");
    }

    #[tokio::test]
    async fn plain_http_needs_the_host_to_be_allowlisted() {
        let denied = Url::parse("http://a.example/").unwrap();
        let tool = fetcher(&["b.example"], true);
        assert!(tool.check_scheme_and_host(&denied).is_err());

        let allowed = Url::parse("http://a.example/").unwrap();
        let tool = fetcher(&["a.example"], true);
        assert!(tool.check_scheme_and_host(&allowed).is_ok());
    }

    #[tokio::test]
    async fn an_unsupported_scheme_is_refused() {
        let tool = fetcher(&["example.com"], true);
        // `file://` has no host, so it fails even the host check.
        let err = fetch(&tool, "file:///etc/passwd")
            .await
            .expect_err("file:// must be refused");
        assert!(matches!(err, Error::Denied(_)), "was: {err}");

        let ftp = Url::parse("ftp://example.com/x").unwrap();
        let err = tool.check_scheme_and_host(&ftp).expect_err("ftp refused");
        assert!(err.to_string().contains("scheme"), "was: {err}");
    }

    #[test]
    fn a_wildcard_entry_grants_https_but_never_plain_http() {
        // Regression for the reviewing pass: the scheme check used to accept
        // `http` on *any* allowlist match, so a glob like `local*` reached a
        // local dev server in the clear. §5.5 and §11.1 both require the
        // *exact* host for `http`, so a wildcard is https-only.
        let tool = fetcher(&["local*"], false);
        assert!(tool.host_allowed("localhost"), "the glob is reachable");

        let https = Url::parse("https://localhost/").unwrap();
        assert!(
            tool.check_scheme_and_host(&https).is_ok(),
            "a wildcard admits https"
        );

        let http = Url::parse("http://localhost/").unwrap();
        let err = tool
            .check_scheme_and_host(&http)
            .expect_err("a wildcard must not grant plain http");
        assert!(err.to_string().contains("exactly"), "was: {err}");
    }

    #[test]
    fn an_exact_entry_grants_plain_http() {
        // The other side of the same rule: naming the host is what buys `http`,
        // which is the local-dev-server exception §11.1 resolves.
        let tool = fetcher(&["localhost"], false);
        let http = Url::parse("http://localhost/").unwrap();
        assert!(tool.check_scheme_and_host(&http).is_ok());
    }

    #[test]
    fn an_ipv6_allowlist_entry_matches_the_bracketed_host() {
        // `Url::host_str` returns `[::1]`, while a user writes `::1`; if the two
        // are not folded together, an exact entry never matches its own literal.
        let tool = fetcher(&["::1"], false);
        assert!(tool.host_allowed("[::1]"));
        assert!(tool.host_named_exactly("[::1]"));

        let http = Url::parse("http://[::1]:8000/").unwrap();
        assert!(
            tool.check_scheme_and_host(&http).is_ok(),
            "an exact `::1` entry should name the host"
        );
    }

    // ----------------------------------------------------------- fetching

    #[tokio::test]
    async fn an_allowlisted_host_is_downloaded() {
        // The positive direction: with the private-IP block off and the host
        // named, the guard lets the request through and the body comes back.
        let base = spawn_server(vec![ok_response("hello from the server")]).await;
        let tool = fetcher(&["127.0.0.1"], false);

        let output = fetch(&tool, &format!("{base}/file.txt"))
            .await
            .expect("an allowed fetch succeeds");

        assert_eq!(output.metadata["status"], 200);
        assert!(!output.truncated);
        let payload: serde_json::Value = serde_json::from_str(&output.content).unwrap();
        assert_eq!(payload["body"], "hello from the server");
        assert_eq!(payload["headers"]["content-type"], "text/plain");
    }

    #[tokio::test]
    async fn a_body_over_the_cap_is_truncated() {
        let body = "a".repeat(4096);
        let base = spawn_server(vec![ok_response(&body)]).await;
        let tool = fetcher(&["127.0.0.1"], false);

        let output = tool
            .invoke(
                ctx(),
                serde_json::json!({ "url": format!("{base}/big"), "max_bytes": 16 }),
            )
            .await
            .expect("fetch");

        assert!(output.truncated, "expected truncation");
        assert_eq!(output.metadata["kept_bytes"], 16);
        let payload: serde_json::Value = serde_json::from_str(&output.content).unwrap();
        assert_eq!(payload["body"].as_str().unwrap().len(), 16);
    }

    #[tokio::test]
    async fn a_redirect_within_the_allowlist_is_followed() {
        let target = spawn_server(vec![ok_response("final destination")]).await;
        let origin = spawn_server(vec![redirect_response(&format!("{target}/final"))]).await;
        let tool = fetcher(&["127.0.0.1"], false);

        let output = fetch(&tool, &format!("{origin}/start"))
            .await
            .expect("a same-host redirect is followed");

        assert_eq!(output.metadata["redirects"], 1);
        let payload: serde_json::Value = serde_json::from_str(&output.content).unwrap();
        assert_eq!(payload["body"], "final destination");
    }

    #[tokio::test]
    async fn a_cross_domain_redirect_is_refused() {
        // The first hop is allowed (`127.0.0.1`), the redirect target is not
        // (`localhost`). Following it would carry the request off the allowed
        // host, so it must stop.
        let origin = spawn_server(vec![redirect_response("http://localhost:9/elsewhere")]).await;
        let tool = fetcher(&["127.0.0.1"], false);

        let err = fetch(&tool, &format!("{origin}/start"))
            .await
            .expect_err("a cross-domain redirect must be refused");

        assert!(matches!(err, Error::Denied(_)), "was: {err}");
        assert!(err.to_string().contains("redirect refused"), "was: {err}");
    }

    #[test]
    fn a_redirect_rewrites_the_method_the_way_http_requires() {
        assert_eq!(Method::Post.after_redirect(303), Method::Get);
        assert_eq!(Method::Post.after_redirect(301), Method::Get);
        assert_eq!(Method::Put.after_redirect(307), Method::Put);
        assert_eq!(Method::Get.after_redirect(302), Method::Get);
    }

    #[test]
    fn client_owned_headers_are_dropped() {
        let headers = BTreeMap::from([
            ("Host".to_string(), "evil".to_string()),
            ("content-length".to_string(), "9".to_string()),
            ("Accept".to_string(), "text/plain".to_string()),
        ]);

        let kept = sanitize_headers(Some(headers));

        assert_eq!(kept, vec![("Accept".to_string(), "text/plain".to_string())]);
    }

    #[test]
    fn the_tool_is_network_risk_and_advertises_its_parameters() {
        let tool = fetcher(&["example.com"], true);
        assert_eq!(tool.risk(), Risk::Network);

        let json = serde_json::to_value(schemars::schema_for!(HttpFetchArgs)).unwrap();
        for field in ["url", "method", "headers", "body", "max_bytes"] {
            assert!(
                json["properties"].get(field).is_some(),
                "the schema should advertise `{field}`: {json}"
            );
        }
        // The method enum is only useful if its spellings reach the model.
        let text = json.to_string();
        for method in ["GET", "POST", "PUT", "DELETE"] {
            assert!(text.contains(method), "the schema should offer {method}");
        }
        assert!(
            json["required"]
                .as_array()
                .is_some_and(|required| required.iter().any(|field| field == "url")),
            "`url` must be required: {json}"
        );
    }
}
