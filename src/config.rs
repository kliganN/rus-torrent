use anyhow::{Context, Result};
use std::{env, fs, path::PathBuf};

#[derive(Clone, Debug)]
pub struct AppConfig {
    pub data_dir: PathBuf,
    pub default_download_dir: PathBuf,
    pub incoming_torrents_dir: PathBuf,
}

impl AppConfig {
    pub fn discover() -> Result<Self> {
        let data_dir = env::current_dir()
            .context("failed to determine the current working directory")?
            .join("rus-torrent-data");

        let config = Self {
            default_download_dir: data_dir.join("downloads"),
            incoming_torrents_dir: data_dir.join("incoming-torrents"),
            data_dir,
        };

        config.prepare_directories()?;
        Ok(config)
    }

    fn prepare_directories(&self) -> Result<()> {
        fs::create_dir_all(&self.data_dir)
            .with_context(|| format!("failed to create {}", self.data_dir.display()))?;
        fs::create_dir_all(&self.default_download_dir)
            .with_context(|| format!("failed to create {}", self.default_download_dir.display()))?;
        fs::create_dir_all(&self.incoming_torrents_dir).with_context(|| {
            format!("failed to create {}", self.incoming_torrents_dir.display())
        })?;
        Ok(())
    }
}
