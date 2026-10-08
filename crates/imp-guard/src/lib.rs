//! `imp-guard`: the wire half of the optional System One guard (SDD §5.6, D16).
//!
//! The policy lives in [`imp_core::guard`]: which commands are eligible, what
//! the thresholds mean, and what the engine does with a verdict. This crate is
//! the part that must touch a socket, kept separate because `imp-core`
//! deliberately has no network dependency.
//!
//! The protocol is one request and one number:
//!
//! ```text
//! POST {base_url}/v1/systemone
//! {"state": "<the command>"}
//!
//! 200 {"unsafe": 0.02}
//! ```
//!
//! `state` is the command string and nothing else (FR-46) — no transcript, no
//! tool output, no file contents. A response is read as a *number*: the decision
//! is a threshold applied in `imp_core`, so a model that asserts it is safe
//! in prose cannot talk its way to an allow (D16). Anything that is not a
//! number in `[0, 1]` under `unsafe` — a bad status, a timeout, a rate limit, an
//! unreadable body, a field that is missing or out of range — is an error, and
//! the engine turns every error into the ordinary prompt (FR-45).

use std::time::Duration;

use async_trait::async_trait;
use imp_core::config::GuardConfig;
use imp_core::error::{Error, Result};
use imp_core::guard::{GuardVerdict, SystemOneGuard};

/// The `System One` guard over HTTP.
pub struct HttpSystemOneGuard {
    /// The outbound client, bounded by the configured timeout.
    client: reqwest::Client,
    /// Fully built `{base_url}/v1/systemone` URL.
    endpoint: String,
}

impl HttpSystemOneGuard {
    /// Build from the `[guard]` config section.
    pub fn new(config: &GuardConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.timeout_secs.max(1)))
            .user_agent(concat!("imp/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("reqwest client construction cannot fail with these settings");
        Self {
            client,
            endpoint: endpoint_for(&config.base_url),
        }
    }
}

#[async_trait]
impl SystemOneGuard for HttpSystemOneGuard {
    async fn verdict(&self, command: &str) -> Result<GuardVerdict> {
        // FR-46: `state` is the command string alone.
        let payload = serde_json::json!({ "state": command });
        let response = self
            .client
            .post(&self.endpoint)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(payload.to_string())
            .send()
            .await
            .map_err(|err| Error::Tool {
                tool: "guard".to_string(),
                message: format!("systemone request failed: {err}"),
            })?;

        let status = response.status();
        if !status.is_success() {
            return Err(Error::Tool {
                tool: "guard".to_string(),
                message: format!("systemone returned {status}"),
            });
        }

        let body = response.text().await.map_err(|err| Error::Tool {
            tool: "guard".to_string(),
            message: format!("cannot read the systemone reply: {err}"),
        })?;
        parse_verdict(&body)
    }
}

/// `{base_url}/v1/systemone`, tolerating a trailing slash.
///
/// `base_url` is the server root, not a provider base URL: the `/v1` prefix is
/// part of the endpoint and must not be repeated in the config.
fn endpoint_for(base_url: &str) -> String {
    format!("{}/v1/systemone", base_url.trim().trim_end_matches('/'))
}

/// Read the unsafe probability out of a response body.
///
/// The contract is deliberately one shape. A field that is missing, a string
/// where a number belongs, a value outside `[0, 1]` or a body that is not JSON
/// at all is an error, and every error becomes a prompt rather than an allow.
fn parse_verdict(body: &str) -> Result<GuardVerdict> {
    let value: serde_json::Value = serde_json::from_str(body).map_err(|err| Error::Tool {
        tool: "guard".to_string(),
        message: format!("systemone returned an unreadable body: {err}"),
    })?;

    let risk = value
        .get("unsafe")
        .and_then(serde_json::Value::as_f64)
        .ok_or_else(|| Error::Tool {
            tool: "guard".to_string(),
            message: "systemone body has no numeric `unsafe` probability".to_string(),
        })?;

    if !risk.is_finite() || !(0.0..=1.0).contains(&risk) {
        return Err(Error::Tool {
            tool: "guard".to_string(),
            message: format!("systemone returned {risk}, which is not a probability"),
        });
    }
    Ok(GuardVerdict { risk: risk as f32 })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::{Arc, Mutex};

    use imp_core::config::Decision;
    use imp_core::guard::GuardThresholds;
    use imp_core::policy::{ApprovalChoice, ApprovalRequest, ApprovalUi, PolicyEngine, ToolGate};
    use imp_core::tool::Risk;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// What the fake server does with one accepted connection.
    enum Canned {
        /// Answer with a status and a body.
        Response(u16, &'static str),
        /// Accept the connection and never answer, to exercise the timeout.
        Silent,
    }

    fn config(base_url: &str) -> GuardConfig {
        GuardConfig {
            enabled: true,
            base_url: base_url.to_string(),
            allow_threshold: 0.25,
            deny_threshold: 0.75,
            timeout_secs: 5,
        }
    }

    fn http_response(status: u16, body: &str) -> String {
        let reason = match status {
            200 => "OK",
            404 => "Not Found",
            429 => "Too Many Requests",
            _ => "Error",
        };
        format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            body.len()
        )
    }

    /// Serve `replies`, one per accepted connection, recording each request
    /// body. Returns the base URL and the recorded bodies. Binds port 0 so
    /// tests never collide, and never touches anything but loopback.
    async fn spawn_server(replies: Vec<Canned>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("local addr");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorder = seen.clone();
        tokio::spawn(async move {
            for reply in replies {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let body = read_request_body(&mut socket).await.unwrap_or_default();
                recorder.lock().unwrap().push(body);
                match reply {
                    Canned::Response(status, body) => {
                        let _ = socket
                            .write_all(http_response(status, body).as_bytes())
                            .await;
                    }
                    Canned::Silent => {
                        // Outlive the client's timeout; the runtime drops the
                        // task when the test ends.
                        tokio::time::sleep(Duration::from_secs(30)).await;
                    }
                }
                let _ = socket.shutdown().await;
            }
        });
        (format!("http://{address}"), seen)
    }

    /// Read one HTTP request and return its body, using `Content-Length`.
    async fn read_request_body(socket: &mut tokio::net::TcpStream) -> std::io::Result<String> {
        let mut buffer = Vec::new();
        let mut scratch = [0u8; 1024];
        loop {
            let read = socket.read(&mut scratch).await?;
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&scratch[..read]);
            if let Some(headers_end) = find(&buffer, b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buffer[..headers_end]).to_string();
                let start = headers_end + 4;
                let length = content_length(&head);
                if buffer.len() >= start + length {
                    return Ok(String::from_utf8_lossy(&buffer[start..start + length]).to_string());
                }
            }
        }
        Ok(String::new())
    }

    fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    fn content_length(head: &str) -> usize {
        head.lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                if name.eq_ignore_ascii_case("content-length") {
                    value.trim().parse().ok()
                } else {
                    None
                }
            })
            .unwrap_or(0)
    }

    #[tokio::test]
    async fn the_state_is_the_command_and_nothing_else() {
        let (base, seen) = spawn_server(vec![Canned::Response(200, r#"{"unsafe":0.02}"#)]).await;
        let guard = HttpSystemOneGuard::new(&config(&base));

        let verdict = guard
            .verdict("curl https://example.com && cat ../secrets")
            .await
            .expect("a low-risk body is a verdict");

        assert_eq!(verdict.risk, 0.02);

        let bodies = seen.lock().unwrap().clone();
        assert_eq!(bodies.len(), 1);
        let payload: serde_json::Value = serde_json::from_str(&bodies[0]).expect("JSON body");
        assert_eq!(
            payload,
            serde_json::json!({ "state": "curl https://example.com && cat ../secrets" }),
            "the payload is the command alone"
        );
        assert_eq!(
            payload.as_object().unwrap().len(),
            1,
            "no transcript, tool output or file contents may ride along (FR-46)"
        );
    }

    #[test]
    fn the_endpoint_is_the_v1_systemone_route() {
        assert_eq!(
            endpoint_for("http://127.0.0.1:8080"),
            "http://127.0.0.1:8080/v1/systemone"
        );
        assert_eq!(
            endpoint_for("https://judge.example/"),
            "https://judge.example/v1/systemone"
        );
        assert_eq!(
            endpoint_for("  https://judge.example  "),
            "https://judge.example/v1/systemone"
        );
    }

    #[tokio::test]
    async fn a_rate_limit_is_an_error_not_a_verdict() {
        let (base, _) = spawn_server(vec![Canned::Response(429, "slow down")]).await;
        let guard = HttpSystemOneGuard::new(&config(&base));

        let err = guard.verdict("curl https://example.com").await.unwrap_err();

        assert!(err.to_string().contains("429"), "was: {err}");
    }

    #[tokio::test]
    async fn a_withdrawn_model_is_an_error() {
        let (base, _) = spawn_server(vec![Canned::Response(404, "no such model")]).await;
        let guard = HttpSystemOneGuard::new(&config(&base));

        let err = guard.verdict("curl https://example.com").await.unwrap_err();

        assert!(err.to_string().contains("404"), "was: {err}");
    }

    #[tokio::test]
    async fn a_timeout_is_an_error() {
        let (base, _) = spawn_server(vec![Canned::Silent]).await;
        let mut settings = config(&base);
        settings.timeout_secs = 1;
        let guard = HttpSystemOneGuard::new(&settings);

        let err = guard.verdict("curl https://example.com").await.unwrap_err();

        assert!(err.to_string().contains("failed"), "was: {err}");
    }

    #[test]
    fn an_unparseable_body_is_an_error() {
        for body in [
            "not json at all",
            "",
            "[]",
            r#"{"unsafe":"0.1"}"#,
            r#"{"risk":0.1}"#,
            r#"{"unsafe":null}"#,
            r#"{"unsafe":1.5}"#,
            r#"{"unsafe":-0.1}"#,
        ] {
            assert!(
                parse_verdict(body).is_err(),
                "`{body}` must not become a verdict"
            );
        }
    }

    #[test]
    fn a_probability_is_read_as_a_verdict() {
        assert_eq!(parse_verdict(r#"{"unsafe":0.0}"#).unwrap().risk, 0.0);
        assert_eq!(parse_verdict(r#"{"unsafe":0.99}"#).unwrap().risk, 0.99);
        assert_eq!(parse_verdict(r#"{"unsafe":1}"#).unwrap().risk, 1.0);
        // Extra fields are ignored; the probability is what matters.
        let verdict = parse_verdict(r#"{"unsafe":0.4,"model":"jev-1.13"}"#).unwrap();
        assert_eq!(verdict.risk, 0.4);
    }

    // --------------------------------------------------- engine + real client

    /// A UI that only records whether it was asked.
    #[derive(Default)]
    struct RecordingUi {
        asked: Mutex<usize>,
    }

    #[async_trait]
    impl ApprovalUi for RecordingUi {
        async fn request(&self, _request: &ApprovalRequest) -> Result<ApprovalChoice> {
            *self.asked.lock().unwrap() += 1;
            Ok(ApprovalChoice::Once)
        }
    }

    /// The two halves together: the real HTTP client inside the real engine,
    /// against a fake model. This is what the task asks a fake server for — a
    /// model that returns probabilities — plus the two guarantees that matter.
    #[tokio::test]
    async fn the_engine_and_the_real_client_resolve_a_prompt() {
        let (base, seen) = spawn_server(vec![Canned::Response(200, r#"{"unsafe":0.01}"#)]).await;
        let ui = Arc::new(RecordingUi::default());
        let engine = PolicyEngine::new(
            Vec::new(),
            Vec::new(),
            Decision::Ask,
            Decision::Deny,
            "/workspace",
            true,
        )
        .with_ui(ui.clone())
        .with_guard(
            Arc::new(HttpSystemOneGuard::new(&config(&base))),
            GuardThresholds {
                allow: 0.25,
                deny: 0.75,
            },
        );

        // Eligible: the guard resolves the prompt, and only the command goes
        // out (FR-46).
        engine
            .check(
                "run_command",
                Risk::Execute,
                &serde_json::json!({ "command": "curl https://example.com" }),
                None,
            )
            .await
            .expect("a confident verdict resolves the prompt");

        assert_eq!(
            *ui.asked.lock().unwrap(),
            0,
            "a confident verdict must not reach the human"
        );
        assert_eq!(
            seen.lock().unwrap().clone(),
            vec![r#"{"state":"curl https://example.com"}"#.to_string()]
        );

        // Ineligible: the floor resolves it before the socket is touched again
        // (FR-44), and the human is asked (FR-43).
        engine
            .check(
                "run_command",
                Risk::Execute,
                &serde_json::json!({ "command": "sudo rm -rf /" }),
                None,
            )
            .await
            .expect("the ordinary prompt is still available");

        assert_eq!(
            *ui.asked.lock().unwrap(),
            1,
            "an ineligible command still prompts"
        );
        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "an ineligible command never reaches the model"
        );
    }
}
