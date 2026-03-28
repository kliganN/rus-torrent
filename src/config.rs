use anyhow::{Context, Result};
use serde::Serialize;
use std::{env, fs, path::PathBuf};

#[derive(Clone, Debug, Serialize)]
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

        Self::discover_in(data_dir)
    }

    pub fn discover_in(data_dir: PathBuf) -> Result<Self> {
        let config = Self {
            default_download_dir: default_home_dir()?,
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

fn default_home_dir() -> Result<PathBuf> {
    match env::var_os("HOME") {
        Some(home) => Ok(PathBuf::from(home)),
        None => env::current_dir().context("failed to determine the home directory"),
    }
}
