//! What a client sends, in the neutral shapes the rest of the crate works with:
//! prompt blocks as one string, and the MCP servers it declared.

use super::*;

pub(super) fn additional_dirs(params: &Value) -> std::result::Result<Vec<PathBuf>, &'static str> {
    let Some(value) = params.get("additionalDirectories").filter(|value| !value.is_null()) else {
        return Ok(Vec::new());
    };
    let Some(values) = value.as_array() else {
        return Err("additionalDirectories must be an array");
    };
    let mut directories = Vec::new();
    for value in values {
        let Some(path) = value.as_str().map(PathBuf::from) else {
            return Err("directory paths must be strings");
        };
        if !path.is_absolute() || !path.is_dir() {
            return Err("workspace directories must be existing absolute directories");
        }
        directories.push(path.canonicalize().map_err(|_| "workspace directory is inaccessible")?);
    }
    directories.sort();
    directories.dedup();
    Ok(directories)
}

/// What the client wants said to the model before anything else. Refused
/// rather than approximated: instructions applied in a mode nobody asked for
/// are a different agent from the one the client meant to open.
pub(super) fn system_prompt(params: &Value) -> std::result::Result<Option<SystemPrompt>, &'static str> {
    let Some(given) = params.get("systemPrompt").filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let replace = match given["mode"].as_str() {
        Some("append") => false,
        Some("override") => true,
        _ => return Err("systemPrompt mode must be append or override"),
    };
    let mut text = Vec::new();
    for block in given["content"].as_array().into_iter().flatten() {
        match (block["type"].as_str(), block["text"].as_str()) {
            (Some("text"), Some(said)) => text.push(said),
            _ => return Err("systemPrompt takes text blocks only"),
        }
    }
    if text.is_empty() {
        return Err("systemPrompt needs content");
    }
    Ok(Some(SystemPrompt { replace, text: text.join("\n\n") }))
}

/// Servers the client named for this session, in the neutral shape an
/// application can act on without knowing this protocol.
pub(super) fn declared(params: &Value, client: &Client) -> Vec<DeclaredServer> {
    let text = |value: &Value, key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
    let pairs = |value: &Value, key: &str| -> Vec<(String, String)> {
        value
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|pair| Some((text(pair, "name")?, text(pair, "value")?)))
            .collect()
    };
    params
        .get("mcpServers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|server| {
            let url = text(server, "url");
            let command = text(server, "command");
            let served = (server["type"] == "acp")
                .then(|| Some(Served { id: text(server, "serverId")?, client: client.clone() }))
                .flatten();
            if url.is_none() && command.is_none() && served.is_none() {
                return None;
            }
            Some(DeclaredServer {
                name: text(server, "name")?,
                command: command.unwrap_or_default(),
                args: server
                    .get("args")
                    .and_then(Value::as_array)
                    .map(|args| args.iter().filter_map(Value::as_str).map(str::to_owned).collect())
                    .unwrap_or_default(),
                env: pairs(server, "env"),
                headers: pairs(server, "headers"),
                url,
                served,
            })
        })
        .collect()
}

/// Turn the client's content blocks into the one string a turn starts from.
///
/// Text arrives as text. A resource — the editor's way of saying "here is the
/// file I am looking at" — is fenced with its own name, so the model reads it
/// as the data it is rather than as instructions from the person. A picture
/// reaches the model by being named in the prompt, so one sent inline is
/// written under the workspace and named.
pub(super) fn prompt_text(params: &Value, cwd: &Path) -> String {
    let mut text = String::new();
    for block in params.get("prompt").and_then(Value::as_array).into_iter().flatten() {
        let field = |name: &str| block.get(name).and_then(Value::as_str);
        match field("type") {
            Some("text") => text.push_str(field("text").unwrap_or_default()),
            Some("resource_link") => {
                if let Some(uri) = field("uri") {
                    text.push_str(uri.strip_prefix("file://").unwrap_or(uri));
                }
            }
            Some("resource") => {
                let resource = &block["resource"];
                let name = resource.get("uri").and_then(Value::as_str).unwrap_or("context");
                if let Some(body) = resource.get("text").and_then(Value::as_str) {
                    text.push_str(&elmee_core::text::fenced("context", &format!(" uri=\"{name}\""), body));
                }
            }
            Some("image") => {
                if let Some(path) = saved_image(cwd, field("mimeType").unwrap_or_default(), field("data")) {
                    text.push_str(&path);
                }
            }
            _ => {}
        }
        text.push('\n');
    }
    text.trim().to_owned()
}

/// Write a pasted picture where the workspace looks for pictures, and say
/// where. Named by content, so pasting the same one twice costs one file.
pub(super) fn saved_image(cwd: &Path, mime: &str, data: Option<&str>) -> Option<String> {
    let bytes = elmee_core::text::unbase64(data?)?;
    // The media type is the client's word, and the file name was built from it: a
    // subtype carrying `/` or `..` chose where the picture landed, and prompt
    // content is not always typed by the person at the keyboard. Take only a
    // plain subtype, up to any structured suffix, and refuse the rest rather
    // than repairing it into something that looks like a name.
    let extension = mime.strip_prefix("image/")?.split('+').next()?;
    let plain = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_';
    if extension.is_empty() || extension.len() > 16 || !extension.bytes().all(plain) {
        return None;
    }
    let relative = format!("media/acp/{:016x}.{extension}", elmee_core::stable_hash(bytes.iter().copied()));
    let path = cwd.join(&relative);
    std::fs::create_dir_all(path.parent()?).ok()?;
    std::fs::write(&path, bytes).ok()?;
    Some(relative)
}
