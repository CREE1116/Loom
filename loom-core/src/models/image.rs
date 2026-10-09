use super::Part;
use anyhow::{Context, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use std::{fs::File, io::Read, path::Path};

const MAX_BYTES: u64 = 20 * 1024 * 1024;

/// Read an image attachment, detect its MIME from bytes, and encode raw base64.
/// Reading is bounded as well as checking metadata, so a growing file cannot
/// bypass the 20 MiB limit.
pub fn image_part(path: &Path) -> anyhow::Result<Part> {
    let file = File::open(path).with_context(|| format!("cannot open image {}", path.display()))?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        bail!("image attachment must be a regular file");
    }
    if metadata.len() > MAX_BYTES {
        bail!("image attachment exceeds 20 MiB");
    }
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_BYTES {
        bail!("image attachment exceeds 20 MiB");
    }
    let mime_type = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        "image/jpeg"
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        "image/gif"
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        "image/webp"
    } else {
        bail!("image attachment must contain PNG, JPEG, GIF, or WebP bytes");
    };
    Ok(Part::Image {
        mime_type: mime_type.into(),
        data: STANDARD.encode(bytes),
    })
}
