//! `imp-update`: the wire half of `imp update` (SDD §9).
//!
//! The rules live in [`imp_core::update`]: what a release looks like, which
//! asset a platform uses, how a checksum is verified, and how the atomic
//! replacement is done. This crate is the one that opens a socket, kept separate
//! because `imp-core` deliberately has no network dependency — the same split
//! `imp-guard` uses.
//!
//! The channel is a GitHub-compatible releases API. One request fetches the
//! latest release, and then one request per asset (the manifest and the binary)
//! fetches the bytes, whose URL comes from the release itself. No token is sent:
//! the repository is public, and a `401`/`403`/`404` is reported as "this works
//! without a token only for a public repository" rather than retried.
//!
//! Only `https`, or a plain `http` to a literal loopback host, is permitted
//! ([`imp_core::update::url_is_permitted`]); the check runs on both the API
//! root and every download URL, so a release cannot redirect the fetch to an
//! insecure scheme.

use std::time::Duration;

use async_trait::async_trait;
use imp_core::config::UpdateConfig;
use imp_core::error::{Error, Result};
use imp_core::update::{
    Asset, ReleaseInfo, UpdateSource, commit_from_notes, is_commit, url_is_permitted,
};
use serde::Deserialize;

/// How long one request may take.
const REQUEST_TIMEOUT_SECS: u64 = 60;

/// A release channel served by a GitHub-compatible API.
pub struct HttpUpdateSource {
    client: reqwest::Client,
    /// API root, without a trailing slash.
    api_url: String,
    /// `owner/name`.
    repo: String,
}

impl HttpUpdateSource {
    /// Build from the `[update]` config section.
    ///
    /// The scheme is refused here rather than at the first call, so a bad
    /// `api_url` is a configuration error and not a failed request.
    pub fn new(config: &UpdateConfig) -> Result<Self> {
        if !url_is_permitted(&config.api_url) {
            return Err(Error::Config(format!(
                "update.api_url must be https (or http on loopback), was `{}`",
                config.api_url
            )));
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .user_agent(concat!("imp/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|err| Error::Provider(format!("cannot build the update client: {err}")))?;
        Ok(Self {
            client,
            api_url: config.api_url.trim().trim_end_matches('/').to_string(),
            repo: config.repo.trim().to_string(),
        })
    }

    /// `{api_url}/repos/{repo}/releases/latest`.
    pub fn latest_url(&self) -> String {
        format!("{}/repos/{}/releases/latest", self.api_url, self.repo)
    }

    /// A request that carries no credential — the repository is public.
    fn request(&self, url: &str) -> Result<reqwest::RequestBuilder> {
        if !url_is_permitted(url) {
            return Err(Error::Refused(format!(
                "the release points at `{url}`, which is not an https URL; refusing to fetch it"
            )));
        }
        Ok(self
            .client
            .get(url)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json"))
    }
}

#[async_trait]
impl UpdateSource for HttpUpdateSource {
    async fn latest(&self) -> Result<ReleaseInfo> {
        let url = self.latest_url();
        let response = self
            .request(&url)?
            .send()
            .await
            .map_err(|err| Error::Provider(format!("cannot reach {url}: {err}")))?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(describe_status(&url, status.as_u16(), &body));
        }

        let body = response
            .text()
            .await
            .map_err(|err| Error::Provider(format!("cannot read {url}: {err}")))?;
        parse_release(&body)
    }

    async fn download(&self, url: &str) -> Result<Vec<u8>> {
        let response = self
            .request(url)?
            .send()
            .await
            .map_err(|err| Error::Provider(format!("cannot download {url}: {err}")))?;

        let status = response.status();
        if !status.is_success() {
            return Err(Error::Provider(format!(
                "downloading {url} failed with {status}"
            )));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|err| Error::Provider(format!("cannot read {url}: {err}")))?;
        Ok(bytes.to_vec())
    }
}

/// The error a non-2xx API response becomes.
///
/// The tokenless case is spelled out because it is the one an operator is most
/// likely to hit: a private repository answers `404` (or `401`), and "not found"
/// on its own reads like a typo in the repository name.
fn describe_status(url: &str, status: u16, body: &str) -> Error {
    let detail = body.trim();
    let detail = if detail.is_empty() {
        String::new()
    } else {
        format!(": {}", truncate(detail, 300))
    };
    match status {
        401 | 403 | 404 => Error::Auth(format!(
            "{url} returned {status}{detail} — without a token the check only works for a \
             public repository"
        )),
        _ => Error::Provider(format!("{url} returned {status}{detail}")),
    }
}

fn truncate(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        text.to_string()
    } else {
        let cut = text
            .char_indices()
            .take_while(|(index, _)| *index < limit)
            .last()
            .map(|(index, ch)| index + ch.len_utf8())
            .unwrap_or(0);
        format!("{}…", &text[..cut])
    }
}

/// The shape of a GitHub/Forgejo release object, as far as imp needs it.
#[derive(Debug, Deserialize)]
struct ApiRelease {
    tag_name: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    target_commitish: Option<String>,
    #[serde(default)]
    assets: Vec<ApiAsset>,
}

#[derive(Debug, Deserialize)]
struct ApiAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: Option<u64>,
}

/// Turn a release JSON body into a [`ReleaseInfo`].
///
/// The commit is taken from `target_commitish` when it is a hex SHA, and from a
/// `build-commit:` trailer in the notes otherwise. If neither is present the
/// commit is `None`, and the update refuses to install — a binary that cannot be
/// tied to a commit is not installed on a guess.
pub fn parse_release(body: &str) -> Result<ReleaseInfo> {
    let release: ApiRelease = serde_json::from_str(body).map_err(|err| {
        Error::Provider(format!(
            "the release API returned an unreadable body: {err}"
        ))
    })?;

    let tag = release.tag_name.trim().to_string();
    if tag.is_empty() {
        return Err(Error::Provider(
            "the release API returned a release with no tag".to_string(),
        ));
    }
    let version = tag.trim_start_matches('v').to_string();

    let commit = release
        .target_commitish
        .as_deref()
        .map(str::trim)
        .filter(|value| is_commit(value))
        .map(|value| value.to_ascii_lowercase())
        .or_else(|| release.body.as_deref().and_then(commit_from_notes));

    let assets = release
        .assets
        .into_iter()
        .map(|asset| Asset {
            name: asset.name,
            url: asset.browser_download_url,
            size: asset.size,
        })
        .collect();

    Ok(ReleaseInfo {
        tag,
        version,
        notes: release.body.filter(|body| !body.trim().is_empty()),
        commit,
        assets,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn config(api_url: &str) -> UpdateConfig {
        UpdateConfig {
            api_url: api_url.to_string(),
            repo: "fixture/imp".to_string(),
            asset_prefix: "imp".to_string(),
        }
    }

    // ------------------------------------------------------------- URL shape

    #[test]
    fn the_latest_url_is_the_github_route() {
        let source = HttpUpdateSource::new(&config("https://api.github.com")).unwrap();
        assert_eq!(
            source.latest_url(),
            "https://api.github.com/repos/fixture/imp/releases/latest"
        );
    }

    #[test]
    fn a_plain_http_api_url_off_loopback_is_refused() {
        assert!(HttpUpdateSource::new(&config("http://example.com")).is_err());
        assert!(HttpUpdateSource::new(&config("http://127.0.0.1:1")).is_ok());
    }

    // ---------------------------------------------------------- the manifest

    #[test]
    fn a_release_body_becomes_the_domain_type() {
        let body = r#"{
            "tag_name": "v0.2.0",
            "body": "notes\nbuild-commit: deadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
            "target_commitish": "main",
            "assets": [
                {"name":"imp-linux-arm64","browser_download_url":"https://example.invalid/a","size":10},
                {"name":"checksums.txt","browser_download_url":"https://example.invalid/c"}
            ]
        }"#;
        let release = parse_release(body).unwrap();
        assert_eq!(release.version, "0.2.0");
        assert_eq!(release.tag, "v0.2.0");
        // `target_commitish` is a branch, so the trailer is what supplies it.
        assert_eq!(
            release.commit.as_deref(),
            Some("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef")
        );
        assert_eq!(release.assets.len(), 2);
        assert_eq!(release.asset("imp-linux-arm64").unwrap().size, Some(10));
    }

    #[test]
    fn a_hex_target_commitish_wins_over_the_trailer() {
        let body = r#"{
            "tag_name": "v0.3.0",
            "body": "build-commit: 1111111111111111111111111111111111111111",
            "target_commitish": "2222222222222222222222222222222222222222",
            "assets": []
        }"#;
        let release = parse_release(body).unwrap();
        assert_eq!(
            release.commit.as_deref(),
            Some("2222222222222222222222222222222222222222")
        );
    }

    #[test]
    fn a_release_with_no_commit_anywhere_has_none() {
        let body = r#"{"tag_name":"v0.4.0","target_commitish":"main","assets":[]}"#;
        assert_eq!(parse_release(body).unwrap().commit, None);
    }

    // ------------------------------------------------------------- over HTTP

    /// One canned HTTP reply per accepted connection.
    enum Canned {
        Response(u16, &'static str),
        Bytes(u16, Vec<u8>),
    }

    /// Serve `replies` on loopback, recording each request line. Binds port 0 so
    /// tests never collide.
    async fn spawn_server(replies: Vec<Canned>) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("addr");
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorder = seen.clone();
        tokio::spawn(async move {
            for reply in replies {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let request = read_request(&mut socket).await.unwrap_or_default();
                recorder.lock().unwrap().push(request);
                match reply {
                    Canned::Response(status, body) => {
                        let _ = socket
                            .write_all(http_response(status, body.as_bytes()).as_bytes())
                            .await;
                    }
                    Canned::Bytes(status, body) => {
                        let _ = socket
                            .write_all(http_response(status, &body).as_bytes())
                            .await;
                    }
                }
                let _ = socket.shutdown().await;
            }
        });
        (format!("http://{address}"), seen)
    }

    fn http_response(status: u16, body: &[u8]) -> String {
        let reason = if status == 200 { "OK" } else { "Error" };
        let head = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let mut out = head.into_bytes();
        out.extend_from_slice(body);
        String::from_utf8_lossy(&out).to_string()
    }

    async fn read_request(socket: &mut tokio::net::TcpStream) -> std::io::Result<String> {
        let mut buffer = Vec::new();
        let mut scratch = [0u8; 1024];
        loop {
            let read = socket.read(&mut scratch).await?;
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&scratch[..read]);
            if buffer.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        Ok(String::from_utf8_lossy(&buffer).to_string())
    }

    #[tokio::test]
    async fn the_latest_release_is_fetched_over_loopback() {
        let body = r#"{"tag_name":"v0.9.0","target_commitish":"1234567890abcdef1234567890abcdef12345678","assets":[{"name":"imp-linux-arm64","browser_download_url":"https://example.invalid/a"}]}"#;
        let (base, seen) = spawn_server(vec![Canned::Response(200, body)]).await;
        let source = HttpUpdateSource::new(&config(&base)).unwrap();

        let release = source.latest().await.unwrap();
        assert_eq!(release.version, "0.9.0");
        assert_eq!(
            release.commit.as_deref(),
            Some("1234567890abcdef1234567890abcdef12345678")
        );

        let requests = seen.lock().unwrap().clone();
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0].starts_with("GET /repos/fixture/imp/releases/latest"),
            "was: {}",
            requests[0]
        );
        // No credential is sent — the repository is public.
        assert!(!requests[0].to_ascii_lowercase().contains("authorization"));
    }

    #[tokio::test]
    async fn a_missing_release_says_the_repository_must_be_public() {
        let (base, _) =
            spawn_server(vec![Canned::Response(404, r#"{"message":"Not Found"}"#)]).await;
        let source = HttpUpdateSource::new(&config(&base)).unwrap();

        let err = source.latest().await.unwrap_err();
        let message = err.to_string();
        assert!(message.contains("404"), "was: {message}");
        assert!(
            message.contains("public repository"),
            "the tokenless limitation must be explicit: {message}"
        );
    }

    #[tokio::test]
    async fn an_asset_is_downloaded_as_bytes() {
        let (base, _) = spawn_server(vec![Canned::Bytes(200, b"binary\x00bytes".to_vec())]).await;
        let source = HttpUpdateSource::new(&config(&base)).unwrap();

        let bytes = source.download(&format!("{base}/assets/a")).await.unwrap();
        assert_eq!(bytes, b"binary\x00bytes");
    }

    #[tokio::test]
    async fn a_download_url_that_is_not_https_is_refused() {
        let source = HttpUpdateSource::new(&config("https://api.github.com")).unwrap();
        let err = source.download("http://example.com/a").await.unwrap_err();
        assert!(err.to_string().contains("not an https URL"), "was: {err}");
    }
}
