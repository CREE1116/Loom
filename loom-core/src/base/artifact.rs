use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    ops::Range,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawRef", into = "RawRef")]
pub struct ArtifactRef {
    pub id: String,
    pub bytes: u64,
}

#[derive(Serialize, Deserialize)]
struct RawRef {
    id: String,
    bytes: u64,
}
impl TryFrom<RawRef> for ArtifactRef {
    type Error = String;
    fn try_from(value: RawRef) -> std::result::Result<Self, Self::Error> {
        validate_id(&value.id).map_err(|e| e.to_string())?;
        Ok(Self {
            id: value.id,
            bytes: value.bytes,
        })
    }
}
impl From<ArtifactRef> for RawRef {
    fn from(value: ArtifactRef) -> Self {
        Self {
            id: value.id,
            bytes: value.bytes,
        }
    }
}

pub trait ArtifactStore: Send + Sync {
    fn put(&self, bytes: &[u8]) -> Result<ArtifactRef>;
    fn get(&self, reference: &ArtifactRef, range: Option<Range<usize>>) -> Result<Vec<u8>>;
}

#[derive(Clone, Debug)]
pub struct FileArtifactStore {
    directory: PathBuf,
}
impl FileArtifactStore {
    pub fn open(workspace: &Path) -> Result<Self> {
        let directory = workspace.join(".loom/artifacts");
        fs::create_dir_all(&directory).context("Creating artifact store")?;
        Ok(Self { directory })
    }
    pub fn path(&self, reference: &ArtifactRef) -> Result<PathBuf> {
        validate_id(&reference.id)?;
        Ok(self.directory.join(&reference.id))
    }
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn validate_id(id: &str) -> Result<()> {
    if id.len() != 64
        || !id
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        bail!("invalid artifact content hash");
    }
    Ok(())
}

impl ArtifactStore for FileArtifactStore {
    fn put(&self, bytes: &[u8]) -> Result<ArtifactRef> {
        let reference = ArtifactRef {
            id: digest(bytes),
            bytes: bytes.len() as u64,
        };
        let path = self.path(&reference)?;
        if path.exists() {
            self.get(&reference, None)?;
            return Ok(reference);
        }
        let mut temporary = tempfile::NamedTempFile::new_in(&self.directory)?;
        temporary.write_all(bytes)?;
        temporary.as_file().sync_all()?;
        match temporary.persist_noclobber(&path) {
            Ok(_) => {}
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                self.get(&reference, None)?;
            }
            Err(error) => return Err(error.error.into()),
        }
        #[cfg(unix)]
        fs::File::open(&self.directory)?.sync_all()?;
        Ok(reference)
    }
    fn get(&self, reference: &ArtifactRef, range: Option<Range<usize>>) -> Result<Vec<u8>> {
        let bytes = fs::read(self.path(reference)?).context("Reading artifact")?;
        if bytes.len() as u64 != reference.bytes || digest(&bytes) != reference.id {
            bail!("artifact content does not match reference {}", reference.id);
        }
        match range {
            Some(range) => bytes
                .get(range)
                .map(|b| b.to_vec())
                .context("artifact byte range out of bounds"),
            None => Ok(bytes),
        }
    }
}
