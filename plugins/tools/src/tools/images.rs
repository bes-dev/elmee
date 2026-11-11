//! Bounded image loading shared by explicit views and prompt attachments.

use std::{fs::File, io::Read, path::Path};

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
