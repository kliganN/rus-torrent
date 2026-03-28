use anyhow::{Context, Result};
use rus_torrent::{
    config::AppConfig,
    path_completion::resolve_user_path,
    torrent::{TorrentEngine, TorrentSnapshot},
};
use serde::Serialize;
use tauri::{Manager, State};

struct DesktopState {
    config: AppConfig,
    engine: TorrentEngine,
}

#[derive(Clone, Debug, Serialize)]
struct QueueTorrentResponse {
    id: usize,
}

#[tauri::command]
fn get_config(state: State<'_, DesktopState>) -> AppConfig {
    state.config.clone()
}

#[tauri::command]
fn list_downloads(state: State<'_, DesktopState>) -> Vec<TorrentSnapshot> {
    state.engine.list_downloads()
}

#[tauri::command]
async fn add_torrent(
    state: State<'_, DesktopState>,
    source: String,
    output_dir: Option<String>,
) -> std::result::Result<QueueTorrentResponse, String> {
    let output_dir = match output_dir.as_deref().map(str::trim) {
        Some(path) if !path.is_empty() => resolve_user_path(path)
            .with_context(|| format!("failed to resolve output directory {path}"))
            .map_err(format_error)?,
        _ => state.config.default_download_dir.clone(),
    };

    let id = state
        .engine
        .add_torrent_source(&source, &output_dir)
        .await
        .map_err(format_error)?;

    Ok(QueueTorrentResponse { id })
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let state = initialize_state(app)?;
            app.manage(state);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_config,
            list_downloads,
            add_torrent
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

fn initialize_state(app: &mut tauri::App) -> Result<DesktopState> {
    let data_dir = app
        .path()
        .app_data_dir()
        .context("failed to resolve the Tauri app data directory")?;
    let config = AppConfig::discover_in(data_dir)?;
    let engine =
        tauri::async_runtime::block_on(TorrentEngine::new(config.default_download_dir.clone()))?;

    Ok(DesktopState { config, engine })
}

fn format_error(error: anyhow::Error) -> String {
    format!("{error:#}")
}
