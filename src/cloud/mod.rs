//! Cursor cloud agents (experimental). See `plans/bork-180/cloud-agents-design.md`.
//!
//! This module only holds the API key and talks to `GET /v1/me` so far.

pub mod auth;
pub mod http;

use std::io::IsTerminal;

use anyhow::bail;

use http::{Client, Me};

/// Every error message here is built from text that has already been through
/// [`auth::ApiKey::redact`], or from text that never saw the key.
#[derive(Debug, thiserror::Error)]
pub enum CloudError {
    #[error("Cursor cloud agents need the macOS keychain, so they only work on macOS for now")]
    Unsupported,

    #[error("no Cursor API key is stored; run `bork cloud login`")]
    NoKey,

    #[error("keychain: {0}")]
    Keychain(String),

    #[error("could not reach Cursor: {0}")]
    Transport(String),

    #[error("Cursor API returned {status} ({}): {message}", code.as_deref().unwrap_or("no error code"))]
    Api {
        status: u16,
        code: Option<String>,
        message: String,
    },

    #[error("unexpected response from Cursor: {0}")]
    Parse(String),
}

/// `bork cloud login`: store a key at `security`'s prompt, then check it.
pub fn login() -> anyhow::Result<()> {
    // Agent panes export BORK_ISSUE_ID. The key shouldn't be typed into a
    // pane an agent is driving.
    if std::env::var_os("BORK_ISSUE_ID").is_some() {
        bail!("run `bork cloud login` in your own terminal, not inside a bork agent session");
    }
    if !std::io::stdin().is_terminal() {
        bail!("`bork cloud login` needs an interactive terminal: the key is typed at the keychain prompt");
    }

    println!("Paste your Cursor API key at the prompts below (twice). It goes straight into the macOS keychain.");
    auth::store_key_interactively()?;
    let key = auth::read_key()?.ok_or(CloudError::NoKey)?;
    let client = Client::new(key);

    match verify(&client) {
        Verified::Ok(me) => {
            println!("{}", describe(&client, &me));
            Ok(())
        }
        Verified::Rejected(err) => {
            let removed = auth::delete_key();
            if matches!(removed, Ok(true)) {
                bail!("Cursor rejected the key, so it was removed from the keychain: {err}");
            }
            bail!(
                "Cursor rejected the key, and removing it failed (run `bork cloud logout`): {err}"
            );
        }
        Verified::Unchecked(err) => {
            bail!("the key is stored, but checking it with Cursor failed: {err}. Run `bork cloud login` again to retry.")
        }
    }
}

/// `bork cloud logout`: delete the keychain item.
pub fn logout() -> anyhow::Result<()> {
    if auth::delete_key()? {
        println!("Removed the Cursor API key from the keychain.");
    } else {
        println!("No Cursor API key was stored.");
    }
    Ok(())
}

enum Verified {
    Ok(Me),
    /// Cursor said the key is bad, so keeping it would only cause failures later.
    Rejected(CloudError),
    /// Couldn't tell (offline, 5xx, odd response). The key may well be fine.
    Unchecked(CloudError),
}

fn verify(client: &Client) -> Verified {
    match client.me() {
        Ok(me) => Verified::Ok(me),
        Err(
            err @ CloudError::Api {
                status: 401 | 403, ..
            },
        ) => Verified::Rejected(err),
        Err(err) => Verified::Unchecked(err),
    }
}

fn describe(client: &Client, me: &Me) -> String {
    let name = me.api_key_name.as_deref().unwrap_or("unnamed key");
    let owner = me.user_email.as_deref().unwrap_or("unknown owner");
    client.redact(&format!(
        "Logged in to Cursor with key \"{name}\" ({owner})."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud::auth::test_key;
    use crate::cloud::http::tests::{fake, SENTINEL};

    fn fake_client(
        status: u16,
        body: &str,
    ) -> (Client, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let (transport, seen) = fake(status, body);
        (Client::with_transport(test_key(SENTINEL), transport), seen)
    }

    #[test]
    fn login_check_only_ever_calls_get_v1_me() {
        for (status, body) in [
            (
                200,
                r#"{"apiKeyName":"laptop","userEmail":"me@example.com"}"#,
            ),
            (
                401,
                r#"{"error":{"code":"unauthorized","message":"bad key"}}"#,
            ),
            (503, "busy"),
        ] {
            let (client, seen) = fake_client(status, body);
            verify(&client);

            let seen = seen.lock().unwrap();
            assert_eq!(seen.len(), 1, "exactly one request");
            assert!(seen[0].contains("url = \"https://api.cursor.com/v1/me\"\n"));
            assert!(seen[0].contains("request = \"GET\"\n"));
            assert!(!seen[0].contains("data"), "no request body");
        }
    }

    #[test]
    fn a_good_key_reports_its_name_and_owner() {
        let (client, _) = fake_client(
            200,
            r#"{"apiKeyName":"laptop","userEmail":"me@example.com"}"#,
        );
        let Verified::Ok(me) = verify(&client) else {
            panic!("expected success");
        };
        assert_eq!(
            describe(&client, &me),
            "Logged in to Cursor with key \"laptop\" (me@example.com)."
        );
    }

    #[test]
    fn unauthorized_and_forbidden_reject_the_key() {
        for status in [401, 403] {
            let (client, _) = fake_client(
                status,
                r#"{"error":{"code":"unauthorized","message":"nope"}}"#,
            );
            assert!(matches!(verify(&client), Verified::Rejected(_)), "{status}");
        }
    }

    #[test]
    fn other_failures_leave_the_key_unchecked() {
        for status in [429, 500, 503] {
            let (client, _) = fake_client(status, "oops");
            assert!(
                matches!(verify(&client), Verified::Unchecked(_)),
                "{status}"
            );
        }
    }

    #[test]
    fn login_output_never_shows_the_key() {
        // A key name that echoes the key itself is absurd, but it's the one
        // response field printed verbatim, so check it's redacted too.
        let body = format!(r#"{{"apiKeyName":"{SENTINEL}","userEmail":"{SENTINEL}@x"}}"#);
        let (client, _) = fake_client(200, &body);
        let Verified::Ok(me) = verify(&client) else {
            panic!("expected success");
        };
        let text = describe(&client, &me);
        assert!(!text.contains(SENTINEL), "leaked: {text}");

        let echo = format!(
            r#"{{"error":{{"code":"unauthorized","message":"key {SENTINEL} is invalid"}}}}"#
        );
        let (client, _) = fake_client(401, &echo);
        let Verified::Rejected(err) = verify(&client) else {
            panic!("expected rejection");
        };
        let mut shown = format!("{err} {err:?}");
        shown.push_str(&format!("{:?}", anyhow::Error::from(err)));
        assert!(!shown.contains(SENTINEL), "leaked: {shown}");
    }
}
