//! Bounded image loading shared by explicit views, prompt attachments and the
//! picture a command has just drawn.

use std::{
    fs::File,
    io::Read,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use elmee_core::message::Image;

use crate::{Error, Result};

const MAX_BYTES: u64 = 5 * 1024 * 1024;

pub(crate) fn load(path: &Path) -> Result<Image> {
    if !path.metadata()?.is_file() {
        return Err(Error::Tool("image must be a regular file".into()));
    }
    let file = File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(Error::Tool("image must be a regular file".into()));
    }
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(Error::Tool(
            "image exceeds 5 MiB; resize it with a system tool, then view the smaller file".into(),
        ));
    }
    let media_type = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        "image/jpeg"
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        "image/gif"
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        "image/webp"
    } else {
        return Err(Error::Tool(
            "unsupported image; use PNG, JPEG, GIF or WebP (convert other formats first)".into(),
        ));
    };
    Ok(Image { media_type: media_type.into(), data: elmee_core::text::base64(&bytes) })
}

pub(crate) fn named_like_image(path: &Path) -> bool {
    path.extension().and_then(|extension| extension.to_str()).is_some_and(|extension| {
        matches!(extension.to_ascii_lowercase().as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp")
    })
}

/// The picture a command drew, when its output names one: the last image path
/// it mentions, inside the workspace and written since `since`. A screenshot
/// tool prints where it saved last; a build that writes many images names
/// them all, and gets one shown, not hundreds.
pub(crate) fn drawn(output: &str, root: &Path, since: SystemTime) -> Option<(String, Image)> {
    // Whole seconds, because a filesystem with coarse timestamps rounds a
    // file written just now down to before the command began.
    let seconds = |time: SystemTime| time.duration_since(UNIX_EPOCH).map(|elapsed| elapsed.as_secs()).ok();
    let named = output
        .split(|c: char| {
            c.is_whitespace() || matches!(c, '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '<' | '>')
        })
        .map(|word| word.trim_end_matches(['.', ',', ';', ':']))
        .rfind(|word| named_like_image(Path::new(word)))?;
    let path = root.join(named).canonicalize().ok().filter(|path| path.starts_with(root))?;
    let modified = path.metadata().and_then(|metadata| metadata.modified()).ok().and_then(seconds)?;
    if modified < seconds(since)? {
        return None;
    }
    load(&path).ok().map(|image| (named.to_owned(), image))
}
