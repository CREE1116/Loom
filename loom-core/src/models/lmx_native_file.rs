//! Native LMX v2 container I/O. Opening reads only the header and bounded JSON index;
//! payloads are read on demand, without a loader, extraction, or cache.

use super::lmx_native::{
    ALIGNMENT, Block, Buffer, HEADER_BYTES, METADATA_LIMIT, Manifest, VERSION,
};
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{self, Seek, SeekFrom, Write},
    path::Path,
};

const MAGIC: &[u8; 8] = b"LOOMLM2\0";
const CHUNK_BYTES: usize = 64 << 10;

#[derive(Debug)]
pub struct NativeFile {
    pub manifest: Manifest,
    pub identity: String,
    pub bytes: u64,
    file: File,
}

impl NativeFile {
    pub fn open(path: &Path) -> Result<Self> {
        let file =
            File::open(path).with_context(|| format!("open native LMX {}", path.display()))?;
        let metadata = file.metadata()?;
        ensure!(metadata.is_file(), "native LMX must be a regular file");
        let bytes = metadata.len();
        ensure!(bytes >= HEADER_BYTES, "truncated native LMX header");
        let mut header = [0u8; HEADER_BYTES as usize];
        read_exact_at(&file, &mut header, 0).context("read native LMX header")?;
        ensure!(&header[..8] == MAGIC, "invalid native LMX magic");
        ensure!(
            u32::from_le_bytes(header[8..12].try_into()?) == VERSION,
            "unsupported native LMX version"
        );
        ensure!(
            u32::from_le_bytes(header[12..16].try_into()?) as u64 == HEADER_BYTES,
            "invalid native LMX header length"
        );
        ensure!(
            header[72..].iter().all(|byte| *byte == 0),
            "nonzero native LMX reserved header bytes"
        );
        let index_offset = u64::from_le_bytes(header[16..24].try_into()?);
        let index_bytes = u64::from_le_bytes(header[24..32].try_into()?);
        let declared_bytes = u64::from_le_bytes(header[32..40].try_into()?);
        ensure!(declared_bytes == bytes, "native LMX file length mismatch");
        ensure!(
            index_offset >= HEADER_BYTES
                && index_offset % ALIGNMENT == 0
                && index_bytes > 0
                && index_bytes <= METADATA_LIMIT
                && index_offset.checked_add(index_bytes) == Some(bytes),
            "invalid native LMX index range or EOF"
        );
        let mut index = vec![0u8; usize::try_from(index_bytes)?];
        read_exact_at(&file, &mut index, index_offset).context("read native LMX index")?;
        ensure!(
            Sha256::digest(&index)[..] == header[40..72],
            "native LMX index checksum mismatch"
        );
        let manifest: Manifest =
            serde_json::from_slice(&index).context("decode native LMX index")?;
        ensure!(
            manifest.version == VERSION,
            "unsupported native LMX manifest version"
        );
        manifest.validate(index_offset)?;
        let identity = manifest.base_identity()?;
        Ok(Self {
            manifest,
            identity,
            bytes,
            file,
        })
    }

    /// Reads exactly one declared block, without moving the file's shared cursor.
    /// On checksum failure the caller must discard the contents of `out`.
    pub fn read_block_into(&self, id: &str, out: &mut [u8]) -> Result<()> {
        let block = self
            .manifest
            .blocks
            .get(id)
            .with_context(|| format!("unknown native LMX block {id}"))?;
        ensure!(
            u64::try_from(out.len())? == block.bytes,
            "native LMX block {id} output size mismatch"
        );
        let mut hasher = Sha256::new();
        let mut offset = block.offset;
        for chunk in out.chunks_mut(CHUNK_BYTES) {
            read_exact_at(&self.file, chunk, offset)
                .with_context(|| format!("read native LMX block {id}"))?;
            hasher.update(&*chunk);
            offset = offset
                .checked_add(chunk.len() as u64)
                .context("native LMX block offset overflow")?;
        }
        check_hash(hasher, block).with_context(|| format!("native LMX block {id}"))
    }

    /// Reads a block-relative range directly into caller-owned storage. This does
    /// not authenticate a partial block; callers can verify the whole block first.
    pub fn read_range_into(&self, range: &Buffer, out: &mut [u8]) -> Result<()> {
        ensure!(
            u64::try_from(out.len())? == range.bytes,
            "native LMX range output size mismatch"
        );
        let block = self
            .manifest
            .blocks
            .get(&range.block)
            .with_context(|| format!("unknown native LMX block {}", range.block))?;
        ensure!(
            range
                .offset
                .checked_add(range.bytes)
                .is_some_and(|end| end <= block.bytes),
            "native LMX range escapes block {}",
            range.block
        );
        let mut offset = block
            .offset
            .checked_add(range.offset)
            .context("native LMX range offset overflow")?;
        for chunk in out.chunks_mut(CHUNK_BYTES) {
            read_exact_at(&self.file, chunk, offset)
                .with_context(|| format!("read native LMX range in {}", range.block))?;
            offset = offset
                .checked_add(chunk.len() as u64)
                .context("native LMX range offset overflow")?;
        }
        Ok(())
    }

    /// Streams all blocks with a fixed 64 KiB buffer. Progress counts payload bytes.
    pub fn verify(&self, progress: &mut dyn FnMut(u64, u64) -> bool) -> Result<()> {
        let total = payload_bytes(&self.manifest)?;
        let mut done = 0;
        check_progress(progress, done, total)?;
        let mut buffer = [0u8; CHUNK_BYTES];
        for (id, block) in &self.manifest.blocks {
            let mut hasher = Sha256::new();
            let mut consumed = 0;
            while consumed < block.bytes {
                let length = (block.bytes - consumed).min(CHUNK_BYTES as u64) as usize;
                let offset = block
                    .offset
                    .checked_add(consumed)
                    .context("native LMX block offset overflow")?;
                read_exact_at(&self.file, &mut buffer[..length], offset)
                    .with_context(|| format!("verify native LMX block {id}"))?;
                hasher.update(&buffer[..length]);
                consumed += length as u64;
                done += length as u64;
                check_progress(progress, done, total)?;
            }
            check_hash(hasher, block).with_context(|| format!("native LMX block {id}"))?;
        }
        check_progress(progress, done, total)
    }
}

/// Writes a new container in the destination directory and publishes without replacement.
/// The emitter receives block IDs in sorted order and must emit exactly `Block::bytes`.
/// Cancellation, including the final progress callback, leaves no destination file.
pub fn write(
    path: &Path,
    mut manifest: Manifest,
    emit: &mut dyn FnMut(&str, &mut dyn Write) -> Result<()>,
    progress: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<NativeFile> {
    ensure!(
        manifest.version == VERSION,
        "unsupported native LMX manifest version"
    );
    match fs::symlink_metadata(path) {
        Ok(_) => anyhow::bail!("native LMX destination already exists: {}", path.display()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("check native LMX destination"),
    }
    let total = payload_bytes(&manifest)?;
    let mut done = 0;
    check_progress(progress, done, total)?;
    // Compute and validate the complete layout before calling the source emitter.
    let mut cursor = HEADER_BYTES;
    for block in manifest.blocks.values_mut() {
        block.offset = aligned(cursor)?;
        block.sha256 = "0".repeat(64);
        cursor = block
            .offset
            .checked_add(block.bytes)
            .context("native LMX block size overflow")?;
    }
    let index_offset = aligned(cursor)?;
    manifest.validate(index_offset)?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut temporary =
        tempfile::NamedTempFile::new_in(parent).context("create native LMX temporary file")?;
    temporary.write_all(&[0u8; HEADER_BYTES as usize])?;
    for (id, block) in &mut manifest.blocks {
        temporary.seek(SeekFrom::Start(block.offset))?;
        let mut writer = BlockWriter {
            file: temporary.as_file_mut(),
            expected: block.bytes,
            written: 0,
            hasher: Sha256::new(),
            done: &mut done,
            total,
            progress,
            failure: None,
        };
        emit(id, &mut writer).with_context(|| format!("emit native LMX block {id}"))?;
        block.sha256 = writer
            .finish()
            .with_context(|| format!("emit native LMX block {id}"))?;
    }
    manifest.validate(index_offset)?;
    let mut index = IndexWriter(Vec::new());
    serde_json::to_writer(&mut index, &manifest).context("encode native LMX index")?;
    let index = index.0;
    let index_bytes = u64::try_from(index.len())?;
    let bytes = index_offset
        .checked_add(index_bytes)
        .context("native LMX file size overflow")?;
    let mut header = [0u8; HEADER_BYTES as usize];
    header[..8].copy_from_slice(MAGIC);
    header[8..12].copy_from_slice(&VERSION.to_le_bytes());
    header[12..16].copy_from_slice(&(HEADER_BYTES as u32).to_le_bytes());
    header[16..24].copy_from_slice(&index_offset.to_le_bytes());
    header[24..32].copy_from_slice(&index_bytes.to_le_bytes());
    header[32..40].copy_from_slice(&bytes.to_le_bytes());
    header[40..72].copy_from_slice(&Sha256::digest(&index));
    temporary.seek(SeekFrom::Start(index_offset))?;
    for chunk in index.chunks(CHUNK_BYTES) {
        temporary.write_all(chunk)?;
        check_progress(progress, done, total)?;
    }
    temporary.as_file().set_len(bytes)?;
    temporary.seek(SeekFrom::Start(0))?;
    temporary.write_all(&header)?;
    temporary
        .as_file()
        .sync_all()
        .context("sync native LMX temporary file")?;
    let native =
        NativeFile::open(temporary.path()).context("validate completed native LMX file")?;
    check_progress(progress, done, total)?;
    // Discard the PersistError's tempfile immediately on failure, so RAII cleanup
    // is complete even if the caller retains the returned error.
    temporary
        .persist_noclobber(path)
        .map_err(|error| error.error)
        .with_context(|| format!("publish native LMX without overwriting {}", path.display()))?;
    Ok(native)
}

fn aligned(value: u64) -> Result<u64> {
    Ok(value
        .checked_add(ALIGNMENT - 1)
        .context("native LMX alignment overflow")?
        / ALIGNMENT
        * ALIGNMENT)
}

fn payload_bytes(manifest: &Manifest) -> Result<u64> {
    manifest.blocks.values().try_fold(0u64, |sum, block| {
        sum.checked_add(block.bytes)
            .context("native LMX payload size overflow")
    })
}

fn check_progress(progress: &mut dyn FnMut(u64, u64) -> bool, done: u64, total: u64) -> Result<()> {
    ensure!(progress(done, total), "native LMX operation cancelled");
    Ok(())
}

fn check_hash(hasher: Sha256, block: &Block) -> Result<()> {
    ensure!(
        format!("{:x}", hasher.finalize()).eq_ignore_ascii_case(&block.sha256),
        "checksum mismatch"
    );
    Ok(())
}

// Positional reads keep independent consumers from racing a shared seek cursor.
fn read_exact_at(file: &File, mut out: &mut [u8], mut offset: u64) -> io::Result<()> {
    #[cfg(not(any(unix, windows)))]
    let mut reader = {
        let mut reader = file.try_clone()?;
        reader.seek(SeekFrom::Start(offset))?;
        reader
    };
    while !out.is_empty() {
        #[cfg(unix)]
        let result = std::os::unix::fs::FileExt::read_at(file, out, offset);
        #[cfg(windows)]
        let result = std::os::windows::fs::FileExt::seek_read(file, out, offset);
        #[cfg(not(any(unix, windows)))]
        let result = std::io::Read::read(&mut reader, out);
        match result {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated native LMX data",
                ));
            }
            Ok(length) => {
                offset = offset.checked_add(length as u64).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "native LMX read offset overflow",
                    )
                })?;
                out = &mut out[length..];
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

struct IndexWriter(Vec<u8>);

impl Write for IndexWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() as u64 > METADATA_LIMIT.saturating_sub(self.0.len() as u64) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "native LMX index exceeds metadata limit",
            ));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct BlockWriter<'a> {
    file: &'a mut File,
    expected: u64,
    written: u64,
    hasher: Sha256,
    done: &'a mut u64,
    total: u64,
    progress: &'a mut dyn FnMut(u64, u64) -> bool,
    // An emitter cannot turn an overrun, cancellation, or I/O failure into success
    // by swallowing the error and returning Ok(()).
    failure: Option<(io::ErrorKind, String)>,
}

impl BlockWriter<'_> {
    fn fail(&mut self, error: io::Error) -> io::Error {
        let (kind, message) = self
            .failure
            .get_or_insert_with(|| (error.kind(), error.to_string()));
        io::Error::new(*kind, message.clone())
    }

    fn finish(self) -> Result<String> {
        if let Some((kind, message)) = self.failure {
            return Err(io::Error::new(kind, message).into());
        }
        ensure!(
            self.written == self.expected,
            "native LMX block size mismatch: expected {}, emitted {}",
            self.expected,
            self.written
        );
        Ok(format!("{:x}", self.hasher.finalize()))
    }
}

impl Write for BlockWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if let Some((kind, message)) = &self.failure {
            return Err(io::Error::new(*kind, message.clone()));
        }
        if bytes.len() as u64 > self.expected - self.written {
            return Err(self.fail(io::Error::new(
                io::ErrorKind::InvalidInput,
                "native LMX block overrun",
            )));
        }
        for chunk in bytes.chunks(CHUNK_BYTES) {
            if let Err(error) = self.file.write_all(chunk) {
                return Err(self.fail(error));
            }
            self.hasher.update(chunk);
            self.written += chunk.len() as u64;
            *self.done += chunk.len() as u64;
            if !(self.progress)(*self.done, self.total) {
                return Err(self.fail(io::Error::other("native LMX operation cancelled")));
            }
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some((kind, message)) = &self.failure {
            return Err(io::Error::new(*kind, message.clone()));
        }
        self.file.flush().map_err(|error| self.fail(error))
    }
}

#[cfg(test)]
mod tests {
    use super::super::lmx_native::fixtures;
    use super::*;

    #[test]
    fn dense_and_moe_roundtrip_with_exact_block_and_range_reads() {
        for moe in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("model.lmx");
            let (manifest, payloads) = fixtures::manifest(moe);
            let identity = manifest.base_identity().unwrap();
            let total: u64 = payloads.values().map(|bytes| bytes.len() as u64).sum();
            let mut reports = Vec::new();
            let native = write(
                &path,
                manifest,
                &mut |id, out| {
                    out.write_all(&payloads[id])?;
                    Ok(())
                },
                &mut |done, total| {
                    reports.push((done, total));
                    true
                },
            )
            .unwrap();
            assert_eq!(native.identity, identity);
            assert_eq!(native.bytes, fs::metadata(&path).unwrap().len());
            assert_eq!(reports.first(), Some(&(0, total)));
            assert_eq!(reports.last(), Some(&(total, total)));
            assert!(reports.windows(2).all(|pair| pair[0].0 <= pair[1].0));
            for (id, bytes) in &payloads {
                assert_eq!(native.manifest.blocks[id].offset % ALIGNMENT, 0);
                let mut out = vec![0; bytes.len()];
                native.read_block_into(id, &mut out).unwrap();
                assert_eq!(&out, bytes);
                let short_len = out.len() - 1;
                assert!(native.read_block_into(id, &mut out[..short_len]).is_err());
            }
            let range = Buffer {
                block: "tokenizer".into(),
                offset: 2,
                bytes: 9,
            };
            let mut out = [0; 9];
            native.read_range_into(&range, &mut out).unwrap();
            assert_eq!(&out, &payloads["tokenizer"][2..11]);
            assert!(native.read_range_into(&range, &mut out[..8]).is_err());
            for (offset, bytes) in [(u64::MAX, 9), (payloads["tokenizer"].len() as u64, 9)] {
                assert!(
                    native
                        .read_range_into(
                            &Buffer {
                                offset,
                                bytes,
                                ..range.clone()
                            },
                            &mut out
                        )
                        .is_err()
                );
            }
            assert!(
                native
                    .read_range_into(
                        &Buffer {
                            block: "missing".into(),
                            ..range
                        },
                        &mut out
                    )
                    .is_err()
            );
            assert!(native.read_block_into("missing", &mut []).is_err());
            let mut verified = (0, 0);
            native
                .verify(&mut |done, total| {
                    verified = (done, total);
                    true
                })
                .unwrap();
            assert_eq!(verified, (total, total));
            assert!(native.verify(&mut |_, _| false).is_err());
            assert_eq!(NativeFile::open(&path).unwrap().identity, identity);
            assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
        }
    }

    #[test]
    fn opening_is_metadata_only_but_reads_and_verification_authenticate_payloads() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("model.lmx");
        let (manifest, payloads) = fixtures::manifest(false);
        let native = write(
            &path,
            manifest,
            &mut |id, out| {
                out.write_all(&payloads[id])?;
                Ok(())
            },
            &mut |_, _| true,
        )
        .unwrap();
        let (id, block) = native.manifest.blocks.first_key_value().unwrap();
        let mut corrupt = fs::OpenOptions::new().write(true).open(&path).unwrap();
        corrupt.seek(SeekFrom::Start(block.offset)).unwrap();
        corrupt.write_all(&[0xff]).unwrap();
        let reopened = NativeFile::open(&path).unwrap();
        let mut out = vec![0; block.bytes as usize];
        assert!(
            format!("{:#}", reopened.read_block_into(id, &mut out).unwrap_err())
                .contains("checksum")
        );
        assert!(
            format!("{:#}", reopened.verify(&mut |_, _| true).unwrap_err()).contains("checksum")
        );
    }

    #[test]
    fn authenticated_index_with_overlapping_blocks_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("model.lmx");
        let (manifest, payloads) = fixtures::manifest(false);
        let native = write(
            &path,
            manifest,
            &mut |id, out| {
                out.write_all(&payloads[id])?;
                Ok(())
            },
            &mut |_, _| true,
        )
        .unwrap();
        let mut manifest = native.manifest;
        let first = manifest.blocks.first_key_value().unwrap().1.offset;
        manifest.blocks.values_mut().nth(1).unwrap().offset = first;
        let index = serde_json::to_vec(&manifest).unwrap();
        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let mut header = [0; HEADER_BYTES as usize];
        read_exact_at(&file, &mut header, 0).unwrap();
        let offset = u64::from_le_bytes(header[16..24].try_into().unwrap());
        header[24..32].copy_from_slice(&(index.len() as u64).to_le_bytes());
        header[32..40].copy_from_slice(&(offset + index.len() as u64).to_le_bytes());
        header[40..72].copy_from_slice(&Sha256::digest(&index));
        file.seek(SeekFrom::Start(offset)).unwrap();
        file.write_all(&index).unwrap();
        file.set_len(offset + index.len() as u64).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        file.write_all(&header).unwrap();
        assert!(
            NativeFile::open(&path)
                .unwrap_err()
                .to_string()
                .contains("overlapping blocks")
        );
    }

    #[test]
    fn cancellation_including_final_callback_cleans_up_without_publication() {
        let directory = tempfile::tempdir().unwrap();
        let (manifest, payloads) = fixtures::manifest(false);
        let mut calls = 0;
        write(
            &directory.path().join("success.lmx"),
            manifest.clone(),
            &mut |id, out| {
                out.write_all(&payloads[id])?;
                Ok(())
            },
            &mut |_, _| {
                calls += 1;
                true
            },
        )
        .unwrap();
        for cancel_at in [1, 2, calls] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("cancelled.lmx");
            let mut seen = 0;
            let result = write(
                &path,
                manifest.clone(),
                &mut |id, out| {
                    // Even a callback that swallows cancellation cannot publish.
                    let _ = out.write_all(&payloads[id]);
                    Ok(())
                },
                &mut |_, _| {
                    seen += 1;
                    seen != cancel_at
                },
            );
            assert!(result.is_err(), "cancel at callback {cancel_at}");
            assert_eq!(seen, cancel_at);
            // Keep the error alive while inspecting cleanup.
            assert!(!path.exists());
            assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
        }
    }

    #[test]
    fn existing_destination_and_publication_race_never_overwrite() {
        let (manifest, payloads) = fixtures::manifest(false);
        for create_during_emit in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("existing.lmx");
            if !create_during_emit {
                fs::write(&path, b"keep me").unwrap();
            }
            let mut emitted = false;
            let result = write(
                &path,
                manifest.clone(),
                &mut |id, out| {
                    if !emitted {
                        fs::write(&path, b"keep me")?;
                        emitted = true;
                    }
                    out.write_all(&payloads[id])?;
                    Ok(())
                },
                &mut |_, _| true,
            );
            assert!(result.is_err());
            assert_eq!(emitted, create_during_emit);
            assert_eq!(fs::read(&path).unwrap(), b"keep me");
            assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
        }
    }

    #[test]
    fn emitter_errors_and_wrong_lengths_clean_up() {
        let (manifest, _) = fixtures::manifest(false);
        for failure in ["error", "short", "overrun"] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("failed.lmx");
            let result = write(
                &path,
                manifest.clone(),
                &mut |id, out| match failure {
                    "error" => anyhow::bail!("emitter failed"),
                    "short" => Ok(()),
                    _ => {
                        let _ = out.write_all(&vec![0; manifest.blocks[id].bytes as usize + 1]);
                        Ok(())
                    }
                },
                &mut |_, _| true,
            );
            assert!(result.is_err());
            assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
        }
    }

    fn header() -> [u8; HEADER_BYTES as usize] {
        let mut header = [0u8; HEADER_BYTES as usize];
        header[..8].copy_from_slice(MAGIC);
        header[8..12].copy_from_slice(&VERSION.to_le_bytes());
        header[12..16].copy_from_slice(&(HEADER_BYTES as u32).to_le_bytes());
        header[16..24].copy_from_slice(&ALIGNMENT.to_le_bytes());
        header[24..32].copy_from_slice(&2u64.to_le_bytes());
        header[32..40].copy_from_slice(&(ALIGNMENT + 2).to_le_bytes());
        header[40..72].copy_from_slice(&Sha256::digest(b"{}"));
        header
    }

    fn open_error(header: &[u8], bytes: u64) -> String {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(header).unwrap();
        file.as_file().set_len(bytes).unwrap();
        file.seek(SeekFrom::Start(ALIGNMENT)).unwrap();
        file.write_all(b"{}").unwrap();
        match NativeFile::open(file.path()) {
            Ok(_) => panic!("malformed header accepted"),
            Err(error) => format!("{error:#}"),
        }
    }

    #[test]
    fn malformed_headers_are_rejected_before_decoding() {
        let cases: &[(usize, &[u8], &str)] = &[
            (0, b"BADMAGIC", "magic"),
            (8, &1u32.to_le_bytes(), "version"),
            (12, &64u32.to_le_bytes(), "header length"),
            (72, &[1], "reserved"),
            (127, &[1], "reserved"),
            (16, &(ALIGNMENT + 1).to_le_bytes(), "index range"),
            (16, &0u64.to_le_bytes(), "index range"),
            (16, &(u64::MAX - ALIGNMENT + 1).to_le_bytes(), "index range"),
            (24, &0u64.to_le_bytes(), "index range"),
            (24, &(METADATA_LIMIT + 1).to_le_bytes(), "index range"),
            (24, &u64::MAX.to_le_bytes(), "index range"),
            (32, &(ALIGNMENT + 3).to_le_bytes(), "file length"),
            (40, &[0; 32], "index checksum"),
        ];
        for &(offset, value, expected) in cases {
            let mut malformed = header();
            malformed[offset..offset + value.len()].copy_from_slice(value);
            let error = open_error(&malformed, ALIGNMENT + 2);
            assert!(error.contains(expected), "offset {offset}: {error}");
        }
    }

    #[test]
    fn truncated_header_and_index_and_trailing_bytes_are_rejected() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&header()[..HEADER_BYTES as usize - 1])
            .unwrap();
        assert!(
            NativeFile::open(file.path())
                .err()
                .unwrap()
                .to_string()
                .contains("truncated")
        );
        file.seek(SeekFrom::Start(0)).unwrap();
        file.write_all(&header()).unwrap();
        for length in [ALIGNMENT + 1, ALIGNMENT + 3] {
            file.as_file().set_len(length).unwrap();
            assert!(
                NativeFile::open(file.path())
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("file length")
            );
        }
    }

    #[test]
    fn emitter_writes_are_chunked_and_hashed() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        let payload = vec![0x5a; CHUNK_BYTES * 2 + 17];
        let mut done = 0;
        let mut reports = Vec::new();
        let mut progress = |done, total| {
            reports.push((done, total));
            true
        };
        let mut writer = BlockWriter {
            file: file.as_file_mut(),
            expected: payload.len() as u64,
            written: 0,
            hasher: Sha256::new(),
            done: &mut done,
            total: payload.len() as u64,
            progress: &mut progress,
            failure: None,
        };
        writer.write_all(&payload).unwrap();
        assert_eq!(
            writer.finish().unwrap(),
            format!("{:x}", Sha256::digest(&payload))
        );
        assert_eq!(fs::read(file.path()).unwrap(), payload);
        assert_eq!(done, payload.len() as u64);
        assert_eq!(
            reports,
            vec![
                (CHUNK_BYTES as u64, done),
                (2 * CHUNK_BYTES as u64, done),
                (done, done),
            ]
        );
    }

    #[test]
    fn emitter_cannot_swallow_overrun_or_cancellation() {
        for cancelled in [false, true] {
            let mut file = tempfile::NamedTempFile::new().unwrap();
            let mut done = 0;
            let mut progress = |_, _| !cancelled;
            let mut writer = BlockWriter {
                file: file.as_file_mut(),
                expected: CHUNK_BYTES as u64,
                written: 0,
                hasher: Sha256::new(),
                done: &mut done,
                total: CHUNK_BYTES as u64,
                progress: &mut progress,
                failure: None,
            };
            let payload = vec![0; CHUNK_BYTES + usize::from(!cancelled)];
            assert!(writer.write_all(&payload).is_err());
            assert!(writer.write(b"").is_err());
            assert!(writer.flush().is_err());
            let error = writer.finish().unwrap_err().to_string();
            assert!(error.contains(if cancelled { "cancelled" } else { "overrun" }));
            assert_eq!(
                file.as_file().metadata().unwrap().len(),
                if cancelled { CHUNK_BYTES as u64 } else { 0 }
            );
        }
    }

    #[test]
    fn short_emission_is_rejected() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        let mut done = 0;
        let mut progress = |_, _| true;
        let mut writer = BlockWriter {
            file: file.as_file_mut(),
            expected: 2,
            written: 0,
            hasher: Sha256::new(),
            done: &mut done,
            total: 2,
            progress: &mut progress,
            failure: None,
        };
        writer.write_all(b"x").unwrap();
        assert!(
            writer
                .finish()
                .unwrap_err()
                .to_string()
                .contains("size mismatch")
        );
    }

    #[test]
    fn index_writer_refuses_to_exceed_limit() {
        let mut writer = IndexWriter(vec![0; METADATA_LIMIT as usize - 1]);
        writer.write_all(b"x").unwrap();
        assert!(writer.write_all(b"y").is_err());
        assert_eq!(writer.0.len() as u64, METADATA_LIMIT);
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn positional_reads_preserve_shared_cursor_and_report_eof() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"abcdef").unwrap();
        file.seek(SeekFrom::Start(3)).unwrap();
        let mut out = [0; 2];
        read_exact_at(file.as_file(), &mut out, 1).unwrap();
        assert_eq!(&out, b"bc");
        assert_eq!(file.stream_position().unwrap(), 3);
        assert_eq!(
            read_exact_at(file.as_file(), &mut out, 5)
                .unwrap_err()
                .kind(),
            io::ErrorKind::UnexpectedEof
        );
    }
}
