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

        self.write(&records)
    }

    /// Removes one paired host by its stable server identifier.
    ///
    /// # Errors
    ///
    /// Returns an error if the store cannot be loaded, serialized, or atomically replaced.
    pub fn remove(&self, server_unique_id: &str) -> Result<()> {
        let mut records = self.load()?;
        records.retain(|candidate| candidate.server_unique_id != server_unique_id);
        self.write(&records)
    }

    fn write(&self, records: &[HostRecord]) -> Result<()> {
        let temporary_path = self.path.with_extension("json.tmp");
        fs::write(&temporary_path, serde_json::to_vec_pretty(records)?)?;
        fs::rename(temporary_path, &self.path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{HostRecord, HostStore};
    use crate::HostAddress;

    #[test]
    fn removes_only_the_requested_host() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("artemis-host-store-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&directory).expect("temporary directory");
        let store = HostStore::new(&directory);
        store.upsert(record("one", "First")).expect("first host");
        store.upsert(record("two", "Second")).expect("second host");

        store.remove("one").expect("remove host");

        let remaining = store.load().expect("remaining hosts");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].server_unique_id, "two");
        fs::remove_dir_all(directory).expect("remove temporary directory");
    }

    fn record(unique_id: &str, name: &str) -> HostRecord {
        HostRecord {
            address: HostAddress::new("192.0.2.1", 47_989),
            name: name.to_owned(),
            server_unique_id: unique_id.to_owned(),
            https_port: 47_984,
            certificate_der: vec![1, 2, 3],
        }
    }
}
