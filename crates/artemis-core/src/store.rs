use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{HostAddress, Result};

/// Persisted certificate pin and host metadata.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HostRecord {
    pub address: HostAddress,
    pub name: String,
    pub server_unique_id: String,
    pub https_port: u16,
    pub certificate_der: Vec<u8>,
}

/// JSON-backed paired-host store.
#[derive(Clone)]
pub struct HostStore {
    path: PathBuf,
}

impl HostStore {
    #[must_use]
    pub fn new(config_dir: impl AsRef<Path>) -> Self {
        Self {
            path: config_dir.as_ref().join("hosts.json"),
        }
    }

    /// Loads all persisted paired-host records.
    ///
    /// # Errors
    ///
    /// Returns an error if the store cannot be read or contains invalid JSON.
    pub fn load(&self) -> Result<Vec<HostRecord>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let data = fs::read(&self.path)?;
        Ok(serde_json::from_slice(&data)?)
    }

    /// Inserts or replaces a paired-host record.
    ///
    /// # Errors
    ///
    /// Returns an error if the store cannot be serialized or atomically replaced.
    pub fn upsert(&self, record: HostRecord) -> Result<()> {
        let mut records = self.load()?;
        records.retain(|candidate| candidate.server_unique_id != record.server_unique_id);
        records.push(record);
        records.sort_by(|left, right| left.name.cmp(&right.name));

        let temporary_path = self.path.with_extension("json.tmp");
        fs::write(&temporary_path, serde_json::to_vec_pretty(&records)?)?;
        fs::rename(temporary_path, &self.path)?;
        Ok(())
    }
}
