//! The things the coding application does without running an agent.
//!
//! Each one answers a question and exits: sign in, list what is available. They live together because they share a
//! shape — read the command line, do one thing, print, return an exit code —
//! and apart from `start` because the front door should read as a list of what
//! elmee can be asked to do, not as the first of them.

use elmee_core::{Error, Result};

/// `Some` when the arguments named a subcommand, whatever it then answered.
pub async fn dispatch(argv: &[String]) -> Option<Result<i32>> {
    let flag = |name: &str| argv.iter().any(|arg| arg == name);
    Some(match argv.first()?.as_str() {
        #[cfg(feature = "agents")]
        "notify" => crate::notify::notify(argv),
        #[cfg(not(feature = "agents"))]
        "notify" => {
            Err(Error::Config("this build runs no project mesh, so there is nobody to notify".into()))
        }
        "login" => login(argv.get(1)).await,
        "logout" => logout(argv.get(1)),
        _ if flag("--list-models") => list_models().await,
        _ => return None,
    })
}

/// Signing in is explicit: a headless model call never stops to ask for a
/// browser or an authorization code.
async fn login(provider: Option<&String>) -> Result<i32> {
    match provider.map_or("codex", String::as_str) {
        "codex" => {
            println!(
                "Opening the sign-in page for Codex. The consent screen says Codex CLI: that is \
                 the public client for terminal sign-in, and there is no way to register another."
            );
            let report = elmee_plugin_provider::auth::codex_login::login(&mut |url| {
                println!("\n{url}\n");
                elmee_plugin_provider::auth::codex_login::open_browser(url);
            })
            .await?;
            println!("{report}. Use it with: elmee -m codex/gpt-5.6-terra");
        }
        other => {
            return Err(Error::Config(format!("unknown login provider {other:?}; expected codex")));
        }
    }
    Ok(0)
}

fn logout(provider: Option<&String>) -> Result<i32> {
    let forgotten = match provider.map_or("codex", String::as_str) {
        "codex" => elmee_plugin_provider::auth::forget(),
        other => {
            return Err(Error::Config(format!("unknown logout provider {other:?}; expected codex")));
        }
    };
    println!("{}", if forgotten { "signed out" } else { "not signed in" });
    Ok(0)
}

/// Every `provider/model` spec, one per line, so a script can pick one without
/// knowing what is configured. One line per model and nothing else on it: this
/// output is read by programs, and the window each endpoint reported is written
/// to the machine's catalogue cache rather than onto the line.
async fn list_models() -> Result<i32> {
    for model in elmee_host::session::models(&std::env::current_dir()?).await? {
        println!("{model}");
    }
    Ok(0)
}
