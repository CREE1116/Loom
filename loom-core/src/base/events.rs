use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecordedEvent<E> {
    pub sequence: u64,
    pub event: E,
}

#[derive(Clone, Debug)]
pub struct EventLog {
    path: PathBuf,
    lock: PathBuf,
}
impl EventLog {
    pub fn open(workspace: &Path) -> Result<Self> {
        let directory = workspace.join(".loom");
        fs::create_dir_all(&directory)?;
        Ok(Self {
            path: directory.join("events.jsonl"),
            lock: directory.join("events.lock"),
        })
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    fn lock(&self) -> Result<File> {
        Ok(OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&self.lock)?)
    }
    pub fn append<E: Serialize>(&self, event: &E) -> Result<u64> {
        let lock = self.lock()?;
        lock.lock_exclusive()?;
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&self.path)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let (records, valid_bytes) = decode::<serde_json::Value>(&bytes)?;
        let sequence = records.last().map_or(Ok(1), |last| {
            last.sequence
                .checked_add(1)
                .context("event sequence overflow")
        })?;
        let mut line = serde_json::to_vec(&RecordedEvent { sequence, event })?;
        line.push(b'\n');
        // A crash can leave only the final record torn. Truncate it under the writer lock.
        file.set_len(valid_bytes as u64)?;
        file.seek(SeekFrom::Start(valid_bytes as u64))?;
        file.write_all(&line)?;
        file.sync_all()?;
        #[cfg(unix)]
        File::open(self.path.parent().context("event log directory missing")?)?.sync_all()?;
        Ok(sequence)
    }
    pub fn read<E: DeserializeOwned>(&self) -> Result<Vec<RecordedEvent<E>>> {
        let lock = self.lock()?;
        FileExt::lock_shared(&lock)?;
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        Ok(decode(&bytes)?.0)
    }
    pub fn fold<E: DeserializeOwned, S>(
        &self,
        mut state: S,
        mut apply: impl FnMut(&mut S, E),
    ) -> Result<S> {
        for record in self.read::<E>()? {
            apply(&mut state, record.event);
        }
        Ok(state)
    }
}

fn decode<E: DeserializeOwned>(bytes: &[u8]) -> Result<(Vec<RecordedEvent<E>>, usize)> {
    let complete = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
    let mut records: Vec<RecordedEvent<E>> = Vec::new();
    if complete == 0 {
        return Ok((records, 0));
    }
    for (index, line) in bytes[..complete - 1].split(|b| *b == b'\n').enumerate() {
        let record: RecordedEvent<E> = serde_json::from_slice(line)
            .with_context(|| format!("Corrupt event record {}", index + 1))?;
        if record.sequence != records.len() as u64 + 1 {
            bail!("event sequence gap at record {}", index + 1);
        }
        records.push(record);
    }
    Ok((records, complete))
}
