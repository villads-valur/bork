//! A tiny HTTP client for the Cursor API, built on `curl -K -`.
//!
//! Everything sensitive (URL, method, headers, the key, the body) goes to curl
//! as a config file on stdin, so `ps` only ever shows the fixed [`CURL_ARGS`].
//! No HTTP crate: bork reaches GitHub through `gh` and Linear through `curl`
//! already.

use std::fmt;
use std::io::{self, Write};
use std::process::{Command, Stdio};

use serde::de::DeserializeOwned;
use serde::Deserialize;

use super::auth::ApiKey;
use super::CloudError;

/// Fixed, not configurable: the key is only ever sent to this host.
const BASE_URL: &str = "https://api.cursor.com";

const MAX_ERROR_BODY: usize = 200;

/// The whole curl argv. Nothing per-request, and so never the key.
///
/// `--disable` has to come first: it stops curl reading `~/.curlrc`, which
/// could otherwise turn on `--verbose` or `--trace` and write the
/// `Authorization` header to stderr or a file.
const CURL_ARGS: &[&str] = &[
    "--disable",
    "--silent",
    "--show-error",
    "--proto",
    "=https",
    "--connect-timeout",
    "10",
    "--max-time",
    "20",
    "--write-out",
    "\\n%{http_code}",
    "--config",
    "-",
];

/// A curl config holding the key. Only its bytes go anywhere, and only to
/// curl's stdin; `Debug` hides the contents.
pub struct CurlConfig(String);

impl CurlConfig {
    pub(super) fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

impl fmt::Debug for CurlConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CurlConfig(***)")
    }
}

/// What one curl run produced. `stdout` is the body followed by `\n` and the
/// HTTP status, from `--write-out`.
pub struct CurlOutput {
    pub success: bool,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Runs curl with a config on stdin. Swapped out in tests so nothing touches
/// the network.
pub type Transport = Box<dyn Fn(&CurlConfig) -> io::Result<CurlOutput> + Send>;

/// The Cursor API, authenticated with one key.
pub struct Client {
    key: ApiKey,
    transport: Transport,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client").field("key", &self.key).finish()
    }
}

/// `GET /v1/me`: who the key belongs to.
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Me {
    pub api_key_name: Option<String>,
    pub user_email: Option<String>,
}

impl Client {
    pub fn new(key: ApiKey) -> Self {
        Self::with_transport(key, Box::new(|config| run_curl("curl", config)))
    }

    pub fn with_transport(key: ApiKey, transport: Transport) -> Self {
        Self { key, transport }
    }

    /// Check the key and see who owns it. Free to call.
    pub fn me(&self) -> Result<Me, CloudError> {
        let body = self.request("GET", "/v1/me", None)?;
        self.parse_json(&body)
    }

    pub fn redact(&self, text: &str) -> String {
        self.key.redact(text)
    }

    fn request(&self, method: &str, path: &str, body: Option<&str>) -> Result<String, CloudError> {
        let config = curl_config(&self.key, method, &format!("{BASE_URL}{path}"), body);
        let output = (self.transport)(&config)
            .map_err(|e| CloudError::Transport(self.redact(&format!("failed to run curl: {e}"))))?;

        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(CloudError::Transport(self.redact(stderr.trim())));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let Some((body, status)) = stdout.rsplit_once('\n') else {
            return Err(CloudError::Parse("curl printed no HTTP status".to_string()));
        };
        let status: u16 = status
            .trim()
            .parse()
            .map_err(|_| CloudError::Parse("curl printed no HTTP status".to_string()))?;

        if (200..300).contains(&status) {
            return Ok(body.to_string());
        }
        Err(self.api_error(status, body))
    }

    /// Turn a non-2xx response into an error. Cursor sends
    /// `{ "error": { "code", "message" } }`; anything else (a proxy's HTML
    /// page, say) is shown as a short excerpt of the body.
    fn api_error(&self, status: u16, body: &str) -> CloudError {
        let envelope = serde_json::from_str::<ErrorEnvelope>(body)
            .ok()
            .and_then(|envelope| envelope.error);
        let (code, message) = match envelope {
            Some(detail) => (detail.code, detail.message.unwrap_or_default()),
            None => (None, self.excerpt(body)),
        };
        CloudError::Api {
            status,
            code: code.map(|code| self.redact(&code)),
            message: self.redact(&message),
        }
    }

    fn parse_json<T: DeserializeOwned>(&self, body: &str) -> Result<T, CloudError> {
        serde_json::from_str(body).map_err(|e| {
            CloudError::Parse(format!(
                "{} (body: {})",
                self.redact(&e.to_string()),
                self.excerpt(body)
            ))
        })
    }

    /// Redact before cutting, so a key that straddles the cut can't leave a
    /// recognisable prefix behind.
    fn excerpt(&self, body: &str) -> String {
        self.redact(body.trim())
            .chars()
            .take(MAX_ERROR_BODY)
            .collect()
    }
}

#[derive(Deserialize)]
struct ErrorEnvelope {
    error: Option<ErrorDetail>,
}

#[derive(Deserialize)]
struct ErrorDetail {
    code: Option<String>,
    message: Option<String>,
}

/// Spawn curl with the fixed argv and write the config to its stdin.
fn run_curl(program: &str, config: &CurlConfig) -> io::Result<CurlOutput> {
    let mut child = Command::new(program)
        .args(CURL_ARGS)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    // Dropping stdin after the write closes it, which is how curl knows the
    // config has ended. Wait for the child even if the write failed, so it
    // isn't left as a zombie.
    let written = match child.stdin.take() {
        Some(mut stdin) => stdin.write_all(config.as_bytes()),
        None => Err(io::Error::other("curl stdin unavailable")),
    };
    let output = child.wait_with_output()?;
    written?;

    Ok(CurlOutput {
        success: output.status.success(),
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

/// One request as a curl config. `user = "<key>:"` makes curl send HTTP Basic
/// auth with the key as the username, which is what Cursor expects.
///
/// `data-raw` rather than `data-binary`: a body starting with `@` would make
/// `data-binary` read a local file and upload it.
fn curl_config(key: &ApiKey, method: &str, url: &str, body: Option<&str>) -> CurlConfig {
    let mut config = String::new();
    push_option(&mut config, "url", url);
    push_option(&mut config, "request", method);
    push_option(&mut config, "user", &format!("{}:", key.expose()));
    push_option(&mut config, "header", "Accept: application/json");
    if let Some(body) = body {
        push_option(&mut config, "header", "Content-Type: application/json");
        push_option(&mut config, "data-raw", body);
    }
    CurlConfig(config)
}

fn push_option(config: &mut String, name: &str, value: &str) {
    config.push_str(name);
    config.push_str(" = \"");
    config.push_str(&escape_config_value(value));
    config.push_str("\"\n");
}

/// Escape for a double-quoted curl config value. curl understands `\\`, `\"`,
/// `\n`, `\r`, `\t` and `\v` there; a raw newline would end the value early.
fn escape_config_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{0b}' => out.push_str("\\v"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::cloud::auth::test_key;
    use std::sync::{Arc, Mutex};

    pub const SENTINEL: &str = "key_SENTINEL_5e81d0";

    /// A transport that records every config it is handed and replies with a
    /// canned status and body.
    pub fn fake(status: u16, body: &str) -> (Transport, Arc<Mutex<Vec<String>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        let stdout = format!("{body}\n{status}").into_bytes();
        let transport: Transport = Box::new(move |config: &CurlConfig| {
            recorder
                .lock()
                .unwrap()
                .push(String::from_utf8(config.as_bytes().to_vec()).unwrap());
            Ok(CurlOutput {
                success: true,
                stdout: stdout.clone(),
                stderr: Vec::new(),
            })
        });
        (transport, seen)
    }

    fn client(status: u16, body: &str) -> (Client, Arc<Mutex<Vec<String>>>) {
        let (transport, seen) = fake(status, body);
        (Client::with_transport(test_key(SENTINEL), transport), seen)
    }

    fn failing_client(stderr: &str) -> Client {
        let stderr = stderr.as_bytes().to_vec();
        Client::with_transport(
            test_key(SENTINEL),
            Box::new(move |_| {
                Ok(CurlOutput {
                    success: false,
                    stdout: Vec::new(),
                    stderr: stderr.clone(),
                })
            }),
        )
    }

    /// Parse a curl config the way curl does: `name = "value"` lines with
    /// backslash escapes inside the quotes.
    fn parse_config(config: &str) -> Vec<(String, String)> {
        config
            .lines()
            .map(|line| {
                let (name, quoted) = line.split_once(" = ").expect("name = value");
                let inner = quoted
                    .strip_prefix('"')
                    .and_then(|rest| rest.strip_suffix('"'))
                    .expect("quoted value");
                let mut value = String::new();
                let mut chars = inner.chars();
                while let Some(c) = chars.next() {
                    if c != '\\' {
                        assert_ne!(c, '"', "unescaped quote in {line}");
                        value.push(c);
                        continue;
                    }
                    match chars.next().expect("dangling backslash") {
                        'n' => value.push('\n'),
                        'r' => value.push('\r'),
                        't' => value.push('\t'),
                        'v' => value.push('\u{0b}'),
                        other => value.push(other),
                    }
                }
                (name.to_string(), value)
            })
            .collect()
    }

    #[test]
    fn me_calls_get_v1_me_with_the_key_in_the_config_only() {
        let (client, seen) = client(
            200,
            r#"{"apiKeyName":"laptop","userEmail":"me@example.com"}"#,
        );

        let me = client.me().unwrap();

        assert_eq!(me.api_key_name.as_deref(), Some("laptop"));
        assert_eq!(me.user_email.as_deref(), Some("me@example.com"));
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        let options = parse_config(&seen[0]);
        assert!(options.contains(&("url".into(), "https://api.cursor.com/v1/me".into())));
        assert!(options.contains(&("request".into(), "GET".into())));
        assert!(options.contains(&("user".into(), format!("{SENTINEL}:"))));
        assert!(!options.iter().any(|(name, _)| name.starts_with("data")));
    }

    #[test]
    fn me_tolerates_missing_and_extra_fields() {
        let (client, _) = client(200, r#"{"createdAt":"2026-01-01","somethingNew":1}"#);
        let me = client.me().unwrap();
        assert!(me.api_key_name.is_none());
        assert!(me.user_email.is_none());
    }

    #[test]
    fn config_round_trips_awkward_bodies() {
        let key = test_key(SENTINEL);
        let bodies = [
            r#"{"prompt":{"text":"say \"hi\""}}"#.to_string(),
            "back\\slash \\n literal and C:\\path\\".to_string(),
            "line one\nline two\r\n\ttabbed\u{0b}".to_string(),
            "æøå 日本語 emoji 🚀 and \u{2028}".to_string(),
            "@/etc/passwd".to_string(),
            "\"".repeat(5),
        ];
        for body in bodies {
            let config = curl_config(&key, "POST", "https://api.cursor.com/v1/x", Some(&body));
            let text = String::from_utf8(config.as_bytes().to_vec()).unwrap();
            // One option per line: an unescaped newline would split the body.
            assert_eq!(text.lines().count(), 6, "{text}");
            let options = parse_config(&text);
            assert!(
                options.contains(&("data-raw".into(), body.clone())),
                "{text}"
            );
        }
    }

    #[test]
    fn argv_is_fixed_and_reads_the_config_from_stdin() {
        assert_eq!(CURL_ARGS.first(), Some(&"--disable"));
        assert!(CURL_ARGS.ends_with(&["--config", "-"]));
        assert!(CURL_ARGS
            .windows(2)
            .any(|pair| pair == ["--proto", "=https"]));
        assert!(CURL_ARGS.contains(&"--max-time"));
    }

    #[test]
    fn error_envelope_is_parsed() {
        let (client, _) = client(
            404,
            r#"{"error":{"code":"agent_not_found","message":"No such agent"}}"#,
        );
        match client.me().unwrap_err() {
            CloudError::Api {
                status,
                code,
                message,
            } => {
                assert_eq!(status, 404);
                assert_eq!(code.as_deref(), Some("agent_not_found"));
                assert_eq!(message, "No such agent");
            }
            other => panic!("expected an API error, got {other:?}"),
        }
    }

    #[test]
    fn a_non_json_error_body_is_shown_as_an_excerpt() {
        let page = format!("<html>{}</html>", "x".repeat(500));
        let (client, _) = client(502, &page);
        match client.me().unwrap_err() {
            CloudError::Api {
                status,
                code,
                message,
            } => {
                assert_eq!(status, 502);
                assert!(code.is_none());
                assert!(message.starts_with("<html>"));
                assert_eq!(message.chars().count(), MAX_ERROR_BODY);
            }
            other => panic!("expected an API error, got {other:?}"),
        }
    }

    #[test]
    fn missing_status_line_is_a_parse_error() {
        let client = Client::with_transport(
            test_key(SENTINEL),
            Box::new(|_| {
                Ok(CurlOutput {
                    success: true,
                    stdout: b"garbage".to_vec(),
                    stderr: Vec::new(),
                })
            }),
        );
        assert!(matches!(client.me().unwrap_err(), CloudError::Parse(_)));
    }

    #[test]
    fn curl_failure_is_a_transport_error() {
        let err = failing_client("curl: (6) Could not resolve host: api.cursor.com")
            .me()
            .unwrap_err();
        assert!(matches!(err, CloudError::Transport(_)));
        assert!(err.to_string().contains("Could not resolve host"));
    }

    #[test]
    fn missing_curl_is_a_transport_error() {
        let client = Client::with_transport(
            test_key(SENTINEL),
            Box::new(|_| Err(io::Error::new(io::ErrorKind::NotFound, "No such file"))),
        );
        assert!(matches!(client.me().unwrap_err(), CloudError::Transport(_)));
    }

    /// Every error path, fed a response that echoes the key back in every form
    /// it could take. None of them may show it.
    #[test]
    fn errors_never_contain_the_key() {
        let basic = crate::cloud::auth::basic_form_for_test(SENTINEL);
        let echo = format!("bad key {SENTINEL} / Basic {basic}");
        // Puts the key across the excerpt cut, so cutting before redacting
        // would leave its first 14 characters behind.
        let long_echo = format!(
            "{}{SENTINEL}{}",
            "y".repeat(MAX_ERROR_BODY - 14),
            "z".repeat(50)
        );

        let errors = vec![
            client(
                401,
                &format!(r#"{{"error":{{"code":"{SENTINEL}","message":"{echo}"}}}}"#),
            )
            .0
            .me()
            .unwrap_err(),
            client(500, &echo).0.me().unwrap_err(),
            client(500, &long_echo).0.me().unwrap_err(),
            client(200, &format!("not json {echo}")).0.me().unwrap_err(),
            client(200, &long_echo).0.me().unwrap_err(),
            failing_client(&format!("curl: (22) {echo}"))
                .me()
                .unwrap_err(),
        ];

        for err in errors {
            for shown in [err.to_string(), format!("{err:?}"), format!("{err:#?}")] {
                assert!(!shown.contains(SENTINEL), "leaked: {shown}");
                assert!(!shown.contains(&basic), "leaked basic form: {shown}");
                // Not even a long prefix of it, which a cut excerpt could leave.
                assert!(!shown.contains(&SENTINEL[..12]), "leaked prefix: {shown}");
            }
        }
    }

    #[test]
    fn debug_of_every_key_holding_type_hides_the_key() {
        let key = test_key(SENTINEL);
        let config = curl_config(&key, "GET", "https://api.cursor.com/v1/me", None);
        let (client, _) = client(200, r#"{"apiKeyName":"laptop"}"#);
        let me = client.me().unwrap();

        let shown = [
            format!("{key:?}"),
            format!("{key}"),
            format!("{config:?}"),
            format!("{client:?}"),
            format!("{client:#?}"),
            format!("{me:?}"),
        ];
        for text in shown {
            assert!(!text.contains(SENTINEL), "leaked: {text}");
        }
    }

    /// Run the real spawn path against a stand-in `curl` script that records
    /// its argv, environment and stdin. The key must arrive on stdin and
    /// nowhere else a process listing or the environment could show it.
    #[test]
    fn real_spawn_keeps_the_key_out_of_argv_and_env() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("curl");
        let out = dir.path();
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{0}/argv'\nenv > '{0}/env'\ncat > '{0}/stdin'\nprintf '{{\"apiKeyName\":\"laptop\"}}\\n200'\n",
                out.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let program = script.to_string_lossy().into_owned();
        let client = Client::with_transport(
            test_key(SENTINEL),
            Box::new(move |config| run_curl(&program, config)),
        );
        let me = client.me().unwrap();
        assert_eq!(me.api_key_name.as_deref(), Some("laptop"));

        let argv = std::fs::read_to_string(out.join("argv")).unwrap();
        let env = std::fs::read_to_string(out.join("env")).unwrap();
        let stdin = std::fs::read_to_string(out.join("stdin")).unwrap();

        assert_eq!(argv.lines().collect::<Vec<_>>(), CURL_ARGS);
        assert!(!argv.contains(SENTINEL), "key in argv: {argv}");
        assert!(!env.contains(SENTINEL), "key in env");
        assert!(
            stdin.contains(&format!("user = \"{SENTINEL}:\"")),
            "{stdin}"
        );
    }
}
