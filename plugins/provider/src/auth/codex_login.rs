//! Signing in to Codex, once, with a ChatGPT account.
//!
//! Kept apart from `auth`, which every request touches: this runs when a person
//! asks for it and needs a browser, a listening socket, and their patience.
//!
//! The flow is the standard one for a program that cannot keep a secret — the
//! authorization code flow with PKCE. elmee invents a random verifier, sends
//! only its hash to the authorization server, and proves possession by
//! producing the original when redeeming the code. So the code is worthless to
//! anyone who intercepts it, which matters here because it arrives over plain
//! HTTP on the loopback interface.
//!
//! The redirect port is fixed at 1455 because the authorization server only
//! accepts redirect URIs registered against the client, and that is the one
//! registered for terminal sign-in.

use std::time::Duration;

use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::{Account, CLIENT_ID, ISSUER, describe, encode_base64url, random, store};
use crate::OrHttp;
use crate::{Error, Result};

const REDIRECT: &str = "http://localhost:1455/auth/callback";
const PORT: u16 = 1455;
/// Long enough to find the browser window and sign in; not so long that a
/// forgotten terminal holds the port all day.
const PATIENCE: Duration = Duration::from_secs(300);

/// Sign in, storing the account. Returns a line describing who was signed in.
///
/// `announce` is handed the URL to visit; the caller decides whether that means
/// printing it, drawing it, or both, and opens a browser if it can.
pub async fn login(announce: &mut impl FnMut(&str)) -> Result<String> {
    let verifier = encode_base64url(&random(64)?);
    let challenge = encode_base64url(sha256(verifier.as_bytes()).as_ref());
    let state = encode_base64url(&random(32)?);

    // Bound before announcing: a browser that arrives first must not find the
    // port closed.
    let listener = TcpListener::bind(("127.0.0.1", PORT)).await.map_err(|error| {
        Error::Config(format!("cannot listen on port {PORT} for the sign-in redirect: {error}"))
    })?;
    announce(&authorize_url(&challenge, &state));

    let code = tokio::time::timeout(PATIENCE, wait_for_code(&listener, &state))
        .await
        .map_err(|_| Error::Config("sign-in timed out".into()))??;
    let mut account = redeem(&code, &verifier).await?;
    store(&account)?;

    account.plan = account.plan.replace('_', " ");
    Ok(match (account.email.is_empty(), account.plan.is_empty()) {
        (false, false) => format!("signed in as {} ({})", account.email, account.plan),
        (false, true) => format!("signed in as {}", account.email),
        _ => "signed in".to_owned(),
    })
}

fn authorize_url(challenge: &str, state: &str) -> String {
    let query = [
        ("response_type", "code"),
        ("client_id", CLIENT_ID),
        ("redirect_uri", REDIRECT),
        ("scope", "openid profile email offline_access"),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("state", state),
    ]
    .map(|(key, value)| format!("{key}={}", escape(value)))
    .join("&");
    format!("{ISSUER}/oauth/authorize?{query}")
}

/// Serve until the browser comes back with the code.
///
/// A browser asks for more than it was sent to — a favicon, a probe — so this
/// answers whatever arrives and keeps waiting until the redirect itself lands.
/// A request bearing the wrong `state` is not this sign-in and is refused: that
/// parameter is the only thing standing between the user and a code someone
/// else's page arranged for them to deliver.
async fn wait_for_code(listener: &TcpListener, state: &str) -> Result<String> {
    loop {
        let (mut stream, _) = listener.accept().await?;
        let mut request = vec![0; 8192];
        let size = stream.read(&mut request).await?;
        let target = String::from_utf8_lossy(&request[..size])
            .split_whitespace()
            .nth(1)
            .unwrap_or_default()
            .to_owned();

        let outcome = match target.split_once('?') {
            Some(("/auth/callback", query)) => {
                let parameter = |name: &str| {
                    query
                        .split('&')
                        .filter_map(|pair| pair.split_once('='))
                        .find(|(key, _)| *key == name)
                        .map(|(_, value)| unescape(value))
                };
                match (parameter("code"), parameter("state")) {
                    (Some(code), Some(returned)) if returned == state => Some(Ok(code)),
                    (_, Some(_)) => Some(Err("this redirect belongs to a different sign-in")),
                    _ => Some(Err(match parameter("error_description").or_else(|| parameter("error")) {
                        Some(_) => "the authorization server refused the sign-in",
                        None => "the redirect carried no authorization code",
                    })),
                }
            }
            _ => None,
        };

        let page = match &outcome {
            Some(Ok(_)) => "Signed in. You can close this tab and return to the terminal.",
            Some(Err(reason)) => reason,
            None => "Waiting for the sign-in redirect.",
        };
        let body = format!("<!doctype html><meta charset=utf-8><title>elmee</title><p>{page}");
        let _ = stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await;
        let _ = stream.shutdown().await;

        match outcome {
            Some(Ok(code)) => return Ok(code),
            Some(Err(reason)) => return Err(Error::Config(reason.to_owned())),
            None => continue,
        }
    }
}

/// Trade the code for tokens, proving with the verifier that this is the same
/// client that asked for it.
async fn redeem(code: &str, verifier: &str) -> Result<Account> {
    let response = crate::http_client()
        .post(format!("{ISSUER}/oauth/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", code),
            ("redirect_uri", REDIRECT),
            ("code_verifier", verifier),
        ])
        .send()
        .await
        .or_http()?;
    let status = response.status();
    let body = response.text().await.or_http()?;
    if !status.is_success() {
        return Err(Error::Provider(format!("sign-in failed ({status}): {body}")));
    }
    let granted: Granted = serde_json::from_str(&body)?;
    let mut account = Account {
        access_token: granted.access_token,
        refresh_token: granted.refresh_token,
        account_id: String::new(),
        email: String::new(),
        plan: String::new(),
    };
    if let Some(id_token) = &granted.id_token {
        describe(&mut account, id_token);
    }
    if account.account_id.is_empty() {
        return Err(Error::Provider(
            "signed in, but the account has no ChatGPT workspace — a subscription is what Codex serves"
                .into(),
        ));
    }
    Ok(account)
}

#[derive(Deserialize)]
struct Granted {
    access_token: String,
    refresh_token: String,
    id_token: Option<String>,
}

/// Percent-encode everything that is not unreserved. Query values here carry
/// base64url and a URL, so `/`, `+` and `:` must not survive unescaped.
fn escape(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (byte as char).to_string(),
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

fn unescape(value: &str) -> String {
    let mut bytes = Vec::with_capacity(value.len());
    let mut rest = value.as_bytes();
    while let Some((first, tail)) = rest.split_first() {
        match first {
            b'%' if tail.len() >= 2 => {
                if let Ok(byte) = u8::from_str_radix(&String::from_utf8_lossy(&tail[..2]), 16) {
                    bytes.push(byte);
                    rest = &tail[2..];
                } else {
                    bytes.push(*first);
                    rest = tail;
                }
            }
            b'+' => {
                bytes.push(b' ');
                rest = tail;
            }
            byte => {
                bytes.push(*byte);
                rest = tail;
            }
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn sha256(data: &[u8]) -> impl AsRef<[u8]> {
    ring::digest::digest(&ring::digest::SHA256, data)
}

/// Ask the desktop to open the sign-in page. Best effort: a machine reached
/// over ssh has no browser, and the URL was printed for exactly that case.
pub fn open_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let _ = std::process::Command::new(opener)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_authorize_url_carries_an_escaped_challenge_and_redirect() {
        let url = authorize_url("ch+al/lenge=", "st/ate");
        assert!(url.starts_with("https://auth.openai.com/oauth/authorize?"), "{url}");
        assert!(url.contains("code_challenge=ch%2Bal%2Flenge%3D"), "{url}");
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback"), "{url}");
        assert!(url.contains("code_challenge_method=S256"), "{url}");
        assert!(url.contains("state=st%2Fate"), "{url}");
    }

    #[test]
    fn escaping_survives_a_round_trip() {
        for value in ["plain", "a/b+c=d", "юникод", "%not-a-pair"] {
            assert_eq!(unescape(&escape(value)), value, "{value}");
        }
        assert_eq!(unescape("a+b"), "a b");
    }

    #[tokio::test]
    async fn the_callback_ignores_stray_requests_and_refuses_a_foreign_state() {
        use tokio::io::AsyncWriteExt;

        async fn visit(port: u16, target: &str) {
            let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            stream
                .write_all(format!("GET {target} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
                .await
                .unwrap();
            let mut page = Vec::new();
            let _ = tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut page).await;
        }

        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let browser = tokio::spawn(async move {
            // What a browser really does before it delivers the redirect.
            visit(port, "/favicon.ico").await;
            visit(port, "/auth/callback?code=the%2Fcode&state=mine").await;
        });
        assert_eq!(wait_for_code(&listener, "mine").await.unwrap(), "the/code");
        browser.await.unwrap();

        let attacker =
            tokio::spawn(async move { visit(port, "/auth/callback?code=theirs&state=theirs").await });
        let refused = wait_for_code(&listener, "mine").await.unwrap_err();
        assert!(refused.to_string().contains("different sign-in"), "{refused}");
        attacker.await.unwrap();
    }

    #[test]
    fn the_challenge_is_the_hash_of_the_verifier() {
        // The one value the authorization server checks: RFC 7636's example.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            encode_base64url(sha256(verifier.as_bytes()).as_ref()),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }
}
