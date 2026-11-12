//! The Cursor API key and the macOS keychain item that holds it.
//!
//! The key is only ever held in an [`ApiKey`], which can't be serialized or
//! printed. Only code in `src/cloud/` can read the string back out, and only
//! `http.rs` does, to hand it to curl.

use std::fmt;
use std::process::{Command, Output, Stdio};

use super::CloudError;

const KEYCHAIN_SERVICE: &str = "bork.cursor-api-key";

/// `security` exits with this when the item doesn't exist.
const SECURITY_NOT_FOUND: i32 = 44;

const REDACTED: &str = "***";

/// A Cursor API key.
///
/// Deliberately has no `Clone`, `Serialize`, `Deserialize`, `Deref`,
/// `AsRef<str>` or `Display`, and `Debug` prints `ApiKey(***)`.
pub struct ApiKey(String);

impl ApiKey {
    /// Wrap a key read from the keychain. Rejects anything that couldn't be a
    /// real key, without echoing what was read.
    fn parse(raw: &str) -> Result<Self, CloudError> {
        let key = raw.trim();
        if key.is_empty() {
            return Err(CloudError::Keychain(
                "the stored Cursor API key is empty".to_string(),
            ));
        }
        // Redaction relies on this: a key with no quotes, backslashes,
        // whitespace or control characters shows up in JSON and stderr
        // exactly as stored, so a plain `replace` finds every echo of it.
        if key
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '"' || c == '\\')
        {
            return Err(CloudError::Keychain(
                "the stored Cursor API key has characters a key can't contain; run `bork cloud login` again"
                    .to_string(),
            ));
        }
        Ok(Self(key.to_string()))
    }

    /// The raw key. Only the HTTP client needs it, to write the curl config.
    pub(super) fn expose(&self) -> &str {
        &self.0
    }

    /// Replace the key, and the base64 forms it takes in an HTTP Basic
    /// `Authorization` header, with `***`.
    pub fn redact(&self, text: &str) -> String {
        let basic_user = base64(format!("{}:", self.0).as_bytes());
        let bare = base64(self.0.as_bytes());
        text.replace(&self.0, REDACTED)
            .replace(&basic_user, REDACTED)
            .replace(&bare, REDACTED)
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKey(***)")
    }
}

/// Read the key from the keychain. `Ok(None)` means none is stored.
pub fn read_key() -> Result<Option<ApiKey>, CloudError> {
    parse_find_output(run_security(&find_args(&account()))?)
}

/// Store a key with `security`'s own prompt. `-w` goes last with no value, so
/// `security` asks for the key on the terminal and it never passes through
/// bork's memory, argv or stdin.
pub fn store_key_interactively() -> Result<(), CloudError> {
    ensure_macos()?;
    let status = Command::new("security")
        .args(add_args(&account()))
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(spawn_error)?;
    if status.success() {
        return Ok(());
    }
    Err(CloudError::Keychain(format!(
        "security could not store the key ({status})"
    )))
}

/// Delete the keychain item. `Ok(false)` means there was nothing to delete.
pub fn delete_key() -> Result<bool, CloudError> {
    let output = run_security(&delete_args(&account()))?;
    Ok(check_security(output, "delete")?.is_some())
}

fn ensure_macos() -> Result<(), CloudError> {
    if cfg!(target_os = "macos") {
        return Ok(());
    }
    Err(CloudError::Unsupported)
}

fn run_security(args: &[&str]) -> Result<Output, CloudError> {
    ensure_macos()?;
    Command::new("security")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(spawn_error)
}

fn spawn_error(err: std::io::Error) -> CloudError {
    CloudError::Keychain(format!("failed to run security: {err}"))
}

/// `Ok(None)` when `security` says the item doesn't exist. Failures report
/// stderr only: on success stdout can be the key itself.
fn check_security(output: Output, verb: &str) -> Result<Option<Output>, CloudError> {
    if output.status.success() {
        return Ok(Some(output));
    }
    if output.status.code() == Some(SECURITY_NOT_FOUND) {
        return Ok(None);
    }
    Err(CloudError::Keychain(format!(
        "security could not {verb} the key: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}

/// Every command names both the service and the account, so they all mean the
/// same item: with only `-s`, a second item stored under another account
/// could be read or deleted instead.
fn account() -> String {
    std::env::var("USER")
        .ok()
        .filter(|user| !user.is_empty())
        .unwrap_or_else(|| "bork".to_string())
}

fn find_args(account: &str) -> [&str; 6] {
    [
        "find-generic-password",
        "-s",
        KEYCHAIN_SERVICE,
        "-a",
        account,
        "-w",
    ]
}

fn add_args(account: &str) -> [&str; 7] {
    [
        "add-generic-password",
        "-U",
        "-s",
        KEYCHAIN_SERVICE,
        "-a",
        account,
        "-w",
    ]
}

fn delete_args(account: &str) -> [&str; 5] {
    [
        "delete-generic-password",
        "-s",
        KEYCHAIN_SERVICE,
        "-a",
        account,
    ]
}

/// stdout of `find-generic-password -w` is the key itself, so it is only ever
/// handed to [`ApiKey::parse`], never put in an error.
fn parse_find_output(output: Output) -> Result<Option<ApiKey>, CloudError> {
    let Some(output) = check_security(output, "read")? else {
        return Ok(None);
    };
    let stdout = String::from_utf8(output.stdout).map_err(|_| {
        CloudError::Keychain("the stored Cursor API key is not valid UTF-8".to_string())
    })?;
    ApiKey::parse(&stdout).map(Some)
}

/// Standard base64 with padding. Only used to recognise the key in a Basic
/// auth header so it can be redacted, so no crate is pulled in for it.
fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(n >> 6) as usize & 63] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[n as usize & 63] as char);
        } else {
            out.push('=');
        }
    }
    out
}

#[cfg(test)]
pub(super) fn test_key(raw: &str) -> ApiKey {
    ApiKey::parse(raw).expect("test key should be valid")
}

#[cfg(test)]
pub(super) fn basic_form_for_test(raw: &str) -> String {
    base64(format!("{raw}:").as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;

    const SENTINEL: &str = "key_SENTINEL_9f3a7c";

    fn output(code: i32, stdout: &str, stderr: &str) -> Output {
        Output {
            status: ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[test]
    fn api_key_has_no_traits_that_could_leak_it() {
        // Compile-time check: each line stops compiling if `ApiKey` gains
        // the trait, because the `_` becomes ambiguous.
        trait AmbiguousIfImpl<A> {
            fn check() {}
        }
        impl<T> AmbiguousIfImpl<()> for T {}
        struct Serializable;
        struct Deserializable;
        struct Borrowable;
        struct Derefs;
        struct Clones;
        struct Displays;
        impl<T: serde::Serialize> AmbiguousIfImpl<Serializable> for T {}
        impl<T: serde::de::DeserializeOwned> AmbiguousIfImpl<Deserializable> for T {}
        impl<T: AsRef<str>> AmbiguousIfImpl<Borrowable> for T {}
        impl<T: std::ops::Deref> AmbiguousIfImpl<Derefs> for T {}
        impl<T: Clone> AmbiguousIfImpl<Clones> for T {}
        impl<T: fmt::Display> AmbiguousIfImpl<Displays> for T {}
        <ApiKey as AmbiguousIfImpl<_>>::check();
    }

    #[test]
    fn debug_hides_the_key() {
        let key = test_key(SENTINEL);
        for text in [format!("{key:?}"), format!("{key:#?}")] {
            assert!(!text.contains(SENTINEL), "leaked: {text}");
            assert_eq!(text, "ApiKey(***)");
        }
    }

    #[test]
    fn redact_hides_the_key_and_its_basic_auth_forms() {
        let key = test_key(SENTINEL);
        let basic = basic_form_for_test(SENTINEL);
        let text = format!(
            "bad key {SENTINEL}, header Authorization: Basic {basic}, bare {}",
            base64(SENTINEL.as_bytes())
        );

        let redacted = key.redact(&text);

        assert!(!redacted.contains(SENTINEL), "{redacted}");
        assert!(!redacted.contains(&basic), "{redacted}");
        assert_eq!(
            redacted,
            "bad key ***, header Authorization: Basic ***, bare ***"
        );
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(b"key:"), "a2V5Og==");
    }

    #[test]
    fn find_output_trims_the_newline_security_prints() {
        let key = parse_find_output(output(0, &format!("{SENTINEL}\n"), ""))
            .unwrap()
            .unwrap();
        assert_eq!(key.expose(), SENTINEL);
    }

    #[test]
    fn find_exit_44_means_no_key() {
        let result = parse_find_output(output(
            44,
            "",
            "security: SecKeychainSearchCopyNext: The specified item could not be found in the keychain.",
        ));
        assert!(matches!(result, Ok(None)));
    }

    #[test]
    fn find_rejects_a_malformed_key_without_echoing_it() {
        let raw = format!("{SENTINEL} {SENTINEL}\n");
        let err = parse_find_output(output(0, &raw, "")).unwrap_err();
        let shown = format!("{err} {err:?}");
        assert!(!shown.contains(SENTINEL), "leaked: {shown}");
    }

    #[test]
    fn find_rejects_an_empty_key() {
        assert!(parse_find_output(output(0, "\n", "")).is_err());
    }

    #[test]
    fn find_failure_reports_stderr_not_stdout() {
        let err =
            parse_find_output(output(1, SENTINEL, "user interaction is not allowed")).unwrap_err();
        let shown = format!("{err} {err:?}");
        assert!(shown.contains("user interaction is not allowed"));
        assert!(!shown.contains(SENTINEL), "leaked: {shown}");
    }

    #[test]
    fn add_puts_w_last_with_no_value_so_security_prompts() {
        // `-w` with a value would put the key in argv; as the last argument
        // with nothing after it, `security` prompts on the terminal instead.
        let args = add_args("someone");
        assert_eq!(args.last(), Some(&"-w"));
        assert!(args.contains(&"-U"));
        assert!(args.windows(2).any(|pair| pair == ["-s", KEYCHAIN_SERVICE]));
    }

    #[test]
    fn find_and_delete_name_the_same_item() {
        let item = ["-s", KEYCHAIN_SERVICE, "-a", "someone"];
        assert_eq!(&add_args("someone")[2..6], item);
        assert_eq!(&find_args("someone")[1..5], item);
        assert_eq!(&delete_args("someone")[1..5], item);
    }
}
