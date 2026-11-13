//! Where the data lives on disk, and safe read-modify-write.
//!
//! Every change (from the TUI or from an agent's CLI call) takes a file lock,
//! re-reads the file, applies the change and writes atomically. That is what
//! lets several programs edit at once without losing each other's changes.

use crate::model::Data;
use anyhow::{Context, Result};
use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;
use std::time::SystemTime;

/// `~/.supersimple`, or `$SUPERSIMPLE_HOME` when set.
pub fn home() -> PathBuf {
    if let Some(h) = std::env::var_os("SUPERSIMPLE_HOME") {
        return PathBuf::from(h);
    }
    let base = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    base.join(".supersimple")
}

pub fn data_path() -> PathBuf {
    home().join("data.json")
}

pub fn load() -> Result<Data> {
    let path = data_path();
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Data::default()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    if text.trim().is_empty() {
        return Ok(Data::default());
    }
    let mut data: Data = serde_json::from_str(&text)
        .with_context(|| format!("{} is not valid Super Simple data", path.display()))?;
    data.normalize();
    Ok(data)
}

fn save(data: &Data) -> Result<()> {
    let path = data_path();
    let tmp = path.with_extension("json.tmp");
    let mut f = File::create(&tmp).with_context(|| format!("writing {}", tmp.display()))?;
    f.write_all(serde_json::to_string_pretty(data)?.as_bytes())?;
    f.write_all(b"\n")?;
    f.sync_all()?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

/// Lock, load the latest data, apply `f`, save. Returns the new data and `f`'s result.
/// Nothing is written when `f` fails.
pub fn mutate<T>(f: impl FnOnce(&mut Data) -> Result<T>) -> Result<(Data, T)> {
    fs::create_dir_all(home())?;
    let lock = File::create(home().join("data.lock"))?;
    lock.lock()?;
    let mut data = load()?;
    let out = f(&mut data)?;
    save(&data)?;
    drop(lock);
    Ok((data, out))
}

/// A cheap fingerprint of the data file, used to notice changes made by others.
pub fn stamp() -> Option<(SystemTime, u64)> {
    let m = fs::metadata(data_path()).ok()?;
    Some((m.modified().ok()?, m.len()))
}
