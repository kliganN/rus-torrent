mod app;
mod config;
mod path_completion;
mod torrent;

use crate::{app::App, config::AppConfig, torrent::TorrentEngine};
use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    let config = AppConfig::discover()?;
    let engine = TorrentEngine::new(config.default_download_dir.clone()).await?;

    let terminal = ratatui::init();
    let result = App::new(config, engine).run(terminal).await;
    ratatui::restore();

    result
}
