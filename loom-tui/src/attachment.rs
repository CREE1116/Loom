//! Local image inputs retain clipboard files until the runtime finishes the turn.
use anyhow::{Context, Result};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Clone, Debug)]
pub struct ImageAttachment {
    path: PathBuf,
    // A queued message and an in-flight command may own the same file.
    _lease: Option<Arc<tempfile::TempPath>>,
}

impl ImageAttachment {
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self> {
        let path = path
            .as_ref()
            .canonicalize()
            .context("이미지 파일을 찾을 수 없습니다")?;
        image::ImageReader::open(&path)?
            .with_guessed_format()?
            .into_dimensions()
            .context("지원하는 이미지 파일이 아닙니다 (PNG, JPEG, GIF, WebP)")?;
        Ok(Self { path, _lease: None })
    }

    pub fn from_clipboard(pixels: arboard::ImageData<'_>) -> Result<Self> {
        let file = tempfile::Builder::new()
            .prefix("loom-image-")
            .suffix(".png")
            .tempfile()?;
        let width = u32::try_from(pixels.width)?;
        let height = u32::try_from(pixels.height)?;
        image::save_buffer_with_format(
            file.path(),
            &pixels.bytes,
            width,
            height,
            image::ColorType::Rgba8,
            image::ImageFormat::Png,
        )?;
        let lease = Arc::new(file.into_temp_path());
        Ok(Self {
            path: lease.to_path_buf(),
            _lease: Some(lease),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn name(&self) -> String {
        if self._lease.is_some() {
            "클립보드.png".into()
        } else {
            self.path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into()
        }
    }
}

/// Only a paste containing exclusively readable image paths becomes attachments.
/// Shell quotes/escapes are used by terminal file dragging, including spaces.
pub fn pasted_paths(text: &str) -> Option<Vec<ImageAttachment>> {
    let text = text.trim();
    // Also allow a plain, unquoted path containing spaces.
    if let Ok(image) = ImageAttachment::from_path(text) {
        return Some(vec![image]);
    }
    let paths = shlex::split(text)?;
    if paths.is_empty() {
        return None;
    }
    paths
        .iter()
        .map(ImageAttachment::from_path)
        .collect::<Result<Vec<_>>>()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn clipboard_file_lives_through_clones_and_is_cleaned_after_last_owner() {
        let image = ImageAttachment::from_clipboard(arboard::ImageData {
            width: 1,
            height: 1,
            bytes: vec![255, 0, 0, 255].into(),
        })
        .unwrap();
        let path = image.path().to_owned();
        let queued = image.clone();
        drop(image);
        assert!(path.exists());
        assert_eq!(image::open(&path).unwrap().width(), 1);
        drop(queued);
        assert!(!path.exists());
    }
}
