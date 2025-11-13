use std::path::Path;
use zksync_os_rocksdb::RocksDB;
use zksync_os_rocksdb::db::NamedColumnFamily;

#[derive(Clone, Copy, Debug)]
pub enum ConfigCF {
    Config,
}

impl ConfigCF {
    fn config_key() -> &'static [u8] {
        b"config"
    }
}

impl NamedColumnFamily for ConfigCF {
    const DB_NAME: &'static str = "config_override";
    const ALL: &'static [Self] = &[ConfigCF::Config];

    fn name(&self) -> &'static str {
        match self {
            ConfigCF::Config => "config",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ConfigDB {
    db: RocksDB<ConfigCF>,
}

impl ConfigDB {
    pub fn new(db_path: &Path) -> Self {
        let db = RocksDB::<ConfigCF>::new(db_path)
            .expect("Failed to open db")
            .with_sync_writes();
        Self { db }
    }

    pub fn read(&self) -> anyhow::Result<serde_yaml::value::Mapping> {
        let bytes = self.db.get_cf(ConfigCF::Config, ConfigCF::config_key())?;

        if let Some(bytes) = bytes {
            let str = String::from_utf8(bytes)?;
            serde_yaml::from_str(&str).map_err(|e| e.into())
        } else {
            Ok(Default::default())
        }
    }

    pub fn write(&self, yaml: &serde_yaml::Mapping) -> anyhow::Result<()> {
        let str = serde_yaml::to_string(yaml)?;
        let mut batch = self.db.new_write_batch();
        batch.put_cf(ConfigCF::Config, ConfigCF::config_key(), str.as_bytes());
        self.db.write(batch)?;
        Ok(())
    }

    pub fn delete(&self) -> anyhow::Result<()> {
        let mut batch = self.db.new_write_batch();
        batch.delete_cf(ConfigCF::Config, ConfigCF::config_key());
        self.db.write(batch)?;
        Ok(())
    }
}
