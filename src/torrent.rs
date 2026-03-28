use crate::path_completion::resolve_user_path;
use anyhow::{anyhow, Context, Result};
use librqbit::{AddTorrent, AddTorrentOptions, Session};
use std::{
    collections::HashMap,
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

#[derive(Clone)]
pub struct TorrentEngine {
    session: Arc<Session>,
    registrations: Arc<Mutex<HashMap<usize, TorrentRegistration>>>,
}

#[derive(Clone, Debug)]
struct TorrentRegistration {
    source: String,
    output_dir: PathBuf,
    submitted_name: String,
}

#[derive(Clone, Debug)]
pub struct TorrentSnapshot {
    pub id: usize,
    pub name: String,
    pub state: String,
    pub progress_ratio: f64,
    pub progress_bytes: u64,
    pub total_bytes: u64,
    pub uploaded_bytes: u64,
    pub finished: bool,
    pub error: Option<String>,
    pub download_speed: Option<String>,
    pub upload_speed: Option<String>,
    pub live_peers: usize,
    pub connecting_peers: usize,
    pub seen_peers: usize,
    pub source: String,
    pub output_dir: PathBuf,
}

#[derive(Clone, Debug)]
pub enum TorrentSource {
    LocalFile(PathBuf),
    RemoteUrl(String),
    Magnet(String),
}

impl TorrentSource {
    pub fn parse(input: &str) -> Result<Self> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Err(anyhow!("torrent source is empty"));
        }

        if is_magnet_link(trimmed) {
            return Ok(Self::Magnet(trimmed.to_string()));
        }

        if is_http_url(trimmed) {
            return Ok(Self::RemoteUrl(trimmed.to_string()));
        }

        Ok(Self::LocalFile(resolve_user_path(trimmed)?))
    }

    pub fn supports_local_completion(input: &str) -> bool {
        let trimmed = input.trim();
        trimmed.is_empty() || (!is_magnet_link(trimmed) && !is_http_url(trimmed))
    }

    pub fn display(&self) -> String {
        match self {
            Self::LocalFile(path) => path.display().to_string(),
            Self::RemoteUrl(url) | Self::Magnet(url) => url.clone(),
        }
    }

    fn into_add_torrent(self) -> Result<AddTorrent<'static>> {
        match self {
            Self::LocalFile(path) => {
                if !path.exists() {
                    return Err(anyhow!("torrent file does not exist: {}", path.display()));
                }

                if !path.is_file() {
                    return Err(anyhow!(
                        "the specified torrent path is not a file: {}",
                        path.display()
                    ));
                }

                if path.extension() != Some(OsStr::new("torrent")) {
                    return Err(anyhow!("expected a .torrent file, got {}", path.display()));
                }

                let torrent_bytes = std::fs::read(&path)
                    .with_context(|| format!("failed to read {}", path.display()))?;

                Ok(AddTorrent::from_bytes(torrent_bytes))
            }
            Self::RemoteUrl(url) | Self::Magnet(url) => Ok(AddTorrent::from_url(url)),
        }
    }

    fn fallback_name(&self) -> String {
        match self {
            Self::LocalFile(path) => file_label(path),
            Self::RemoteUrl(url) => url
                .rsplit('/')
                .next()
                .filter(|segment| !segment.is_empty())
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| "remote-torrent".to_string()),
            Self::Magnet(_) => "magnet-download".to_string(),
        }
    }
}

impl TorrentEngine {
    pub async fn new(default_output_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&default_output_dir).with_context(|| {
            format!(
                "failed to create the default download directory {}",
                default_output_dir.display()
            )
        })?;

        // librqbit requires a session-level output directory even when per-torrent
        // output folders are overridden via AddTorrentOptions.
        let session = Session::new(default_output_dir.clone())
            .await
            .with_context(|| {
                format!(
                    "failed to initialize the BitTorrent session for {}",
                    default_output_dir.display()
                )
            })?;

        Ok(Self {
            session,
            registrations: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub async fn add_torrent_source(
        &self,
        source_input: &str,
        output_dir: impl AsRef<Path>,
    ) -> Result<usize> {
        let source = TorrentSource::parse(source_input)?;
        let output_dir = output_dir.as_ref();
        let source_display = source.display();

        std::fs::create_dir_all(output_dir)
            .with_context(|| format!("failed to create {}", output_dir.display()))?;

        let response = self
            .session
            .add_torrent(
                source.clone().into_add_torrent()?,
                Some(AddTorrentOptions {
                    overwrite: true,
                    output_folder: Some(output_dir.to_string_lossy().into_owned()),
                    ..Default::default()
                }),
            )
            .await
            .with_context(|| format!("failed to add {source_display}"))?;

        let handle = response
            .into_handle()
            .ok_or_else(|| anyhow!("librqbit returned no managed handle for the torrent"))?;

        let id = handle.id();
        let fallback_name = source.fallback_name();

        self.registrations()
            .entry(id)
            .or_insert_with(|| TorrentRegistration {
                source: source_display,
                output_dir: output_dir.to_path_buf(),
                submitted_name: handle.name().unwrap_or(fallback_name),
            });

        Ok(id)
    }

    pub fn list_downloads(&self) -> Vec<TorrentSnapshot> {
        let registrations = self.registrations().clone();

        let mut downloads = self.session.with_torrents(|torrents| {
            let mut items = Vec::new();

            for (id, handle) in torrents {
                let stats = handle.stats();
                let (download_speed, upload_speed, live_peers, connecting_peers, seen_peers) =
                    match stats.live.as_ref() {
                        Some(live) => (
                            Some(live.download_speed.to_string()),
                            Some(live.upload_speed.to_string()),
                            live.snapshot.peer_stats.live,
                            live.snapshot.peer_stats.connecting,
                            live.snapshot.peer_stats.seen,
                        ),
                        None => (None, None, 0, 0, 0),
                    };
                let registration = registrations.get(&id).cloned().unwrap_or_else(|| {
                    let fallback_name = handle
                        .name()
                        .unwrap_or_else(|| format!("torrent-{}", handle.id()));
                    TorrentRegistration {
                        source: "<unknown>".to_string(),
                        output_dir: PathBuf::from("<session-default>"),
                        submitted_name: fallback_name,
                    }
                });

                let total_bytes = stats.total_bytes;
                let progress_ratio = if total_bytes == 0 {
                    0.0
                } else {
                    stats.progress_bytes as f64 / total_bytes as f64
                };

                items.push(TorrentSnapshot {
                    id,
                    name: handle
                        .name()
                        .unwrap_or_else(|| registration.submitted_name.clone()),
                    state: handle.with_state(|state| state.name().to_owned()),
                    progress_ratio: progress_ratio.clamp(0.0, 1.0),
                    progress_bytes: stats.progress_bytes,
                    total_bytes,
                    uploaded_bytes: stats.uploaded_bytes,
                    finished: stats.finished,
                    error: stats.error.clone(),
                    download_speed,
                    upload_speed,
                    live_peers,
                    connecting_peers,
                    seen_peers,
                    source: registration.source,
                    output_dir: registration.output_dir,
                });
            }

            items
        });

        downloads.sort_by_key(|download| download.id);
        downloads
    }

    fn registrations(&self) -> std::sync::MutexGuard<'_, HashMap<usize, TorrentRegistration>> {
        self.registrations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];

    let mut value = bytes as f64;
    let mut unit = 0usize;

    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn file_label(path: &Path) -> String {
    path.file_stem()
        .and_then(|value| value.to_str())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| "unnamed-torrent".to_string())
}

fn is_http_url(value: &str) -> bool {
    let lowercase = value.to_ascii_lowercase();
    lowercase.starts_with("http://") || lowercase.starts_with("https://")
}

fn is_magnet_link(value: &str) -> bool {
    value.to_ascii_lowercase().starts_with("magnet:")
}
