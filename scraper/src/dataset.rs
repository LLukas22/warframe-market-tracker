//! Local dataset transactions. Readers hold a shared lock; publication and recovery take
//! an exclusive lock. A durable journal makes multi-table replacement restartable.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

const JOURNAL: &str = "storage/transaction.json";

/// Zstd level for every parquet file the scraper writes: fast, and most of the gain.
pub const PARQUET_ZSTD_LEVEL: i32 = 3;

pub fn unique_id() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{}-{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap(),
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

pub struct DatasetLock(File);

impl DatasetLock {
    fn open(root: &Path) -> Result<File> {
        fs::create_dir_all(root.join("storage"))?;
        Ok(OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join("storage/lock"))?)
    }

    pub fn read(root: &Path) -> Result<Self> {
        loop {
            let file = Self::open(root)?;
            file.lock_shared()?;
            if !root.join(JOURNAL).exists() {
                return Ok(Self(file));
            }
            drop(file);
            // A writer died during publication. Finish it before exposing the dataset.
            drop(Self::write(root)?);
        }
    }

    pub fn write(root: &Path) -> Result<Self> {
        let file = Self::open(root)?;
        file.lock()?;
        recover(root)?;
        // Unpublished staging directories cannot contain committed data.
        let staging = root.join("storage/staging");
        if staging.exists() {
            fs::remove_dir_all(staging)?;
        }
        Ok(Self(file))
    }
}

impl Drop for DatasetLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

pub fn sync_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    Ok(())
}

fn sync_ancestors(path: &Path, root: &Path) -> Result<()> {
    let mut dir = path;
    loop {
        sync_dir(dir)?;
        if dir == root {
            return Ok(());
        }
        dir = dir.parent().context("dataset path is outside its root")?;
    }
}

pub fn durable_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().unwrap();
    fs::create_dir_all(parent)?;
    let tmp = path.with_extension("tmp");
    let mut file = File::create(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(tmp, path)?;
    sync_dir(parent)
}

#[derive(Default, Serialize, Deserialize)]
struct Journal {
    replacements: Vec<(PathBuf, PathBuf)>,
    deletions: Vec<PathBuf>,
}

/// Caller must retain the exclusive dataset lock until commit has returned.
pub struct Transaction {
    root: PathBuf,
    stage: PathBuf,
    journal: Journal,
}

impl Transaction {
    pub fn new(root: &Path) -> Result<Self> {
        let stage = PathBuf::from("storage/staging").join(unique_id());
        fs::create_dir_all(root.join(&stage))?;
        sync_dir(&root.join("storage"))?;
        sync_dir(&root.join("storage/staging"))?;
        Ok(Self {
            root: root.into(),
            stage,
            journal: Journal::default(),
        })
    }

    pub fn stage(&mut self, destination: PathBuf) -> PathBuf {
        let staged = self.stage.join(self.journal.replacements.len().to_string());
        self.journal
            .replacements
            .push((staged.clone(), destination));
        self.root.join(staged)
    }

    pub fn delete(&mut self, path: PathBuf) {
        self.journal.deletions.push(path);
    }

    pub fn commit(self) -> Result<()> {
        for (staged, _) in &self.journal.replacements {
            File::open(self.root.join(staged))?.sync_all()?;
        }
        sync_dir(&self.root.join(&self.stage))?;
        durable_write(
            &self.root.join(JOURNAL),
            &serde_json::to_vec(&self.journal)?,
        )?;
        recover(&self.root)?;
        fs::remove_dir_all(self.root.join(self.stage))?;
        Ok(())
    }
}

fn recover(root: &Path) -> Result<()> {
    let path = root.join(JOURNAL);
    let journal: Journal = match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("reading dataset transaction")?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    // Publish all replacements before removing any inputs. Each step is idempotent.
    for (staged, destination) in &journal.replacements {
        let staged = root.join(staged);
        let destination = root.join(destination);
        if staged.exists() {
            let parent = destination.parent().unwrap();
            fs::create_dir_all(parent)?;
            fs::rename(staged, &destination)?;
            sync_ancestors(parent, root)?;
        } else {
            ensure!(
                destination.exists(),
                "missing transaction output {}",
                destination.display()
            );
        }
    }
    for deletion in &journal.deletions {
        let deletion = root.join(deletion);
        match fs::remove_file(&deletion) {
            Ok(()) => sync_dir(deletion.parent().unwrap())?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    fs::remove_file(path)?;
    sync_dir(&root.join("storage"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reader_recovers_partially_published_transaction() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().to_path_buf();
        fs::write(root.join("old"), b"old")?;
        let mut tx = Transaction::new(&root)?;
        let first = tx.stage("first".into());
        let second = tx.stage("second".into());
        fs::write(&first, b"one")?;
        fs::write(&second, b"two")?;
        tx.delete("old".into());
        // Simulate a crash after the first rename, with a durable journal still present.
        durable_write(&root.join(JOURNAL), &serde_json::to_vec(&tx.journal)?)?;
        fs::rename(first, root.join("first"))?;
        let guard = DatasetLock::read(&root)?;
        assert_eq!(fs::read(root.join("first"))?, b"one");
        assert_eq!(fs::read(root.join("second"))?, b"two");
        assert!(!root.join("old").exists());
        assert!(!root.join(JOURNAL).exists());
        drop(guard);
        Ok(())
    }
}
