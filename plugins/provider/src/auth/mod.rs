//! A signed-in Codex account: what was stored, and keeping it usable.
//!
//! Codex rather than ChatGPT, because that is what is being signed in to: the
//! OAuth client is Codex's, the endpoint is Codex's, and which models are
//! served is Codex's decision. A ChatGPT subscription is what pays for it.
//!
//! Credentials live in `~/.elmee/auth.json` and nowhere else — no other tool's
//! file is read or written, so elmee works on a machine that has never had one
//! installed, and cannot invalidate anyone else's session by rotating a token
//! out from under it.
//!
//! An access token is short-lived and a refresh token is single-use: the server
//! hands back a new one each time and retires the old. That makes persisting
//! the result of a refresh not an optimisation but the thing that keeps the
//! account signed in, and it is why the write happens before the token is
//! returned to the caller.

pub mod codex_login;

use std::{
    path::PathBuf,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::OrHttp;
use crate::{Error, Result};

pub(crate) const ISSUER: &str = "https://auth.openai.com";
/// Codex's public client for terminal sign-in. There is no way to register
/// another against a ChatGPT subscription, so the consent screen names Codex
/// rather than elmee — `login` says so plainly rather than hiding it.
pub(crate) const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// Renew this long before the token actually expires, so a turn that starts
/// valid does not become invalid halfway through a long stream.
const RENEW_WITHIN: u64 = 10 * 60;

#[derive(Clone, Serialize, Deserialize)]
pub struct Account {
    pub access_token: String,
    pub refresh_token: String,
    #[serde(default)]
    pub account_id: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub plan: String,
}

pub fn path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".elmee/auth.json"))
}

pub fn stored() -> Option<Account> {
    let data = std::fs::read(path()?).ok()?;
    serde_json::from_slice(&data).ok()
}

/// Write the account, readable only by its owner: this file is the account.
pub fn store(account: &Account) -> Result<()> {
    let path = path().ok_or_else(|| Error::Config("no HOME to store credentials in".into()))?;
    store_at(&path, account)
}

fn store_at(path: &std::path::Path, account: &Account) -> Result<()> {
    let data = serde_json::to_vec_pretty(account)?;
    let options = elmee_plugin_atomic::Options { mode: Some(0o600), ..Default::default() };
    elmee_plugin_atomic::write(path, &data, options)?;
    Ok(())
}

pub fn forget() -> bool {
    path().is_some_and(|path| std::fs::remove_file(path).is_ok())
}

/// The account to make a request with, renewed first if it is near expiry.
pub async fn current() -> Result<Account> {
    let account = stored().ok_or_else(|| {
        Error::SignIn("not signed in to Codex: run `elmee login` (or use an API key)".into())
    })?;
    if expiry(&account.access_token).is_some_and(|expires_at| expires_at > now() + RENEW_WITHIN) {
        return Ok(account);
    }
    let _lock = RefreshLock::acquire("auth.refresh.lock", "Codex").await?;
    let account = stored().ok_or_else(|| Error::Config("Codex sign-in disappeared during refresh".into()))?;
    if expiry(&account.access_token).is_some_and(|expires_at| expires_at > now() + RENEW_WITHIN) {
        Ok(account)
    } else {
        renew(&account).await
    }
}

pub(super) struct RefreshLock(PathBuf);

impl RefreshLock {
    /// Hold the right to refresh one account's credentials.
    ///
    /// A lock file older than a quarter of an hour belonged to a process that
    /// is gone, so it is taken rather than waited on forever.
    pub(super) async fn acquire(file: &str, whose: &str) -> Result<Self> {
        let path = path()
            .and_then(|path| path.parent().map(|parent| parent.join(file)))
            .ok_or_else(|| Error::Config("no HOME to lock credentials in".into()))?;
        for _ in 0..300 {
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(_) => return Ok(Self(path)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = std::fs::metadata(&path)
                        .and_then(|metadata| metadata.modified())
                        .ok()
                        .and_then(|modified| modified.elapsed().ok())
                        .is_some_and(|age| age > std::time::Duration::from_secs(15 * 60));
                    if stale {
                        let _ = std::fs::remove_file(&path);
                    } else {
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
                Err(error) => return Err(error.into()),
            }
        }
        Err(Error::Config(format!("timed out waiting for another process to refresh {whose} credentials")))
    }
}

impl Drop for RefreshLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Exchange the refresh token for a fresh pair and keep the result.
pub async fn renew(account: &Account) -> Result<Account> {
    let response = crate::http_client()
        .post(format!("{ISSUER}/oauth/token"))
        .json(&serde_json::json!({
            "client_id": CLIENT_ID,
            "grant_type": "refresh_token",
            "refresh_token": account.refresh_token,
        }))
        .send()
        .await
        .or_http()?;
    let status = response.status();
    let body = response.text().await.or_http()?;
    if !status.is_success() {
        return Err(Error::Provider(format!("sign-in expired ({status}); run `elmee login` again: {body}")));
    }
    let renewed: Renewed = serde_json::from_str(&body)?;
    let mut account = Account {
        access_token: renewed.access_token,
        // A response without a new refresh token leaves the old one in force.
        refresh_token: renewed.refresh_token.unwrap_or_else(|| account.refresh_token.clone()),
        ..account.clone()
    };
    if let Some(id_token) = &renewed.id_token {
        describe(&mut account, id_token);
    }
    store(&account)?;
    Ok(account)
}

#[derive(Deserialize)]
struct Renewed {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
}

/// Fill in who this is from the identity token's claims. The account id is
/// what the API needs; the plan and address are only so `login` can say who it
/// signed in as, which is the difference between a working setup and one that
/// silently used the wrong workspace.
pub(crate) fn describe(account: &mut Account, id_token: &str) {
    let Some(claims) = claims(id_token) else { return };
    if let Some(email) = claims.get("email").and_then(Value::as_str) {
        account.email = email.to_owned();
    }
    let Some(auth) = claims.get("https://api.openai.com/auth") else { return };
    if let Some(id) = auth.get("chatgpt_account_id").and_then(Value::as_str) {
        account.account_id = id.to_owned();
    }
    if let Some(plan) = auth.get("chatgpt_plan_type").and_then(Value::as_str) {
        account.plan = plan.to_owned();
    }
}

/// When the token stops being accepted, if it says.
fn expiry(token: &str) -> Option<u64> {
    claims(token)?.get("exp").and_then(Value::as_u64)
}

/// The middle segment of a JWT, decoded. Nothing is verified here: the server
/// checks the signature, and reading a claim we then hand straight back to it
/// gains nothing from us checking too.
fn claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    serde_json::from_slice(&base64url(payload)?).ok()
}

pub(crate) fn base64url(text: &str) -> Option<Vec<u8>> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut bytes = Vec::with_capacity(text.len() * 3 / 4);
    let (mut accumulator, mut bits) = (0_u32, 0_u32);
    for byte in text.bytes().filter(|byte| *byte != b'=') {
        let value = ALPHABET.iter().position(|candidate| *candidate == byte)? as u32;
        accumulator = (accumulator << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            bytes.push((accumulator >> bits) as u8);
        }
    }
    Some(bytes)
}

pub(crate) fn encode_base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut text = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let block = chunk.iter().fold(0_u32, |block, byte| (block << 8) | u32::from(*byte));
        let block = block << (8 * (3 - chunk.len()));
        for index in 0..=chunk.len() {
            text.push(ALPHABET[(block >> (18 - 6 * index)) as usize & 0x3f] as char);
        }
    }
    text
}

pub(crate) fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

/// Random bytes from the kernel. Two of these decide whether a sign-in can be
/// intercepted, which rules out anything derived from the clock or the pid.
pub(crate) fn random(count: usize) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut bytes = vec![0; count];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes)
}

/// A provider session's identity. Separate sessions must not share routing
/// hints just because an ACP host keeps one process warm.
pub fn new_session_id() -> String {
    let bytes = random(16).unwrap_or_else(|_| now().to_le_bytes().repeat(2));
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{}-{}-4{}-a{}-{}", &hex[..8], &hex[8..12], &hex[13..16], &hex[17..20], &hex[20..32])
}

/// Legacy process identity for callers without a session.
pub fn session_id() -> String {
    static ID: Mutex<Option<String>> = Mutex::new(None);
    ID.lock().unwrap().get_or_insert_with(new_session_id).clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_round_trips_and_reads_jwt_claims() {
        for case in [b"".as_slice(), b"a", b"ab", b"abc", b"abcd", &[0xff, 0x00, 0xfe]] {
            let encoded = encode_base64url(case);
            assert!(!encoded.contains('='), "{encoded}");
            assert_eq!(base64url(&encoded).as_deref(), Some(case), "{encoded}");
        }

        let payload = encode_base64url(
            br#"{"email":"a@b.c","exp":99,"https://api.openai.com/auth":{"chatgpt_account_id":"acc","chatgpt_plan_type":"pro"}}"#,
        );
        let token = format!("header.{payload}.signature");
        let mut account = Account {
            access_token: token.clone(),
            refresh_token: String::new(),
            account_id: String::new(),
            email: String::new(),
            plan: String::new(),
        };
        describe(&mut account, &token);
        assert_eq!((account.account_id.as_str(), account.plan.as_str()), ("acc", "pro"));
        assert_eq!(account.email, "a@b.c");
        assert_eq!(expiry(&token), Some(99));
        assert_eq!(expiry("not a jwt"), None);
    }

    #[test]
    fn credentials_are_replaced_atomically_with_owner_only_permissions() {
        let root = std::env::temp_dir().join(format!("elmee-auth-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let path = root.join("auth.json");
        let account = Account {
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            account_id: "account".into(),
            email: "user@example.test".into(),
            plan: "test".into(),
        };
        store_at(&path, &account).unwrap();
        let stored: Account = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(stored.refresh_token, "refresh");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1, "temporary credential file leaked");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_session_id_is_stable_and_shaped_like_the_endpoint_expects() {
        let id = session_id();
        assert_eq!(id, session_id());
        let parts: Vec<usize> = id.split('-').map(str::len).collect();
        assert_eq!(parts, [8, 4, 4, 4, 12], "{id}");
    }
}
