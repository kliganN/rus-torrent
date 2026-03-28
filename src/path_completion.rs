use anyhow::{Context, Result};
use std::{
    env, fs,
    io::ErrorKind,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PathCompletionMode {
    TorrentFile,
    Directory,
}

#[derive(Clone, Debug)]
pub struct CompletionCandidate {
    pub replacement: String,
    pub is_dir: bool,
    pub is_remote_hint: bool,
}

impl CompletionCandidate {
    pub fn remote_hint() -> Self {
        Self {
            replacement: "URL/magnet".to_string(),
            is_dir: false,
            is_remote_hint: true,
        }
    }

    pub fn kind_label(&self) -> &'static str {
        if self.is_remote_hint {
            "link"
        } else if self.is_dir {
            "dir"
        } else {
            "torrent"
        }
    }
}

#[derive(Clone, Debug)]
pub struct CompletionSet {
    pub seed_input: String,
    pub candidates: Vec<CompletionCandidate>,
}

pub fn collect_candidates(input: &str, mode: PathCompletionMode) -> Result<CompletionSet> {
    let seed_input = input.trim().to_string();
    let lookup = Lookup::from_input(&seed_input)?;

    if !lookup.search_dir.is_dir() {
        return Ok(CompletionSet {
            seed_input,
            candidates: Vec::new(),
        });
    }

    let read_dir = match fs::read_dir(&lookup.search_dir) {
        Ok(read_dir) => read_dir,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::NotFound | ErrorKind::PermissionDenied | ErrorKind::InvalidInput
            ) =>
        {
            return Ok(CompletionSet {
                seed_input,
                candidates: Vec::new(),
            });
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to read completion directory {}",
                    lookup.search_dir.display()
                )
            });
        }
    };

    let mut candidates = Vec::new();

    for entry in read_dir.flatten() {
        let entry_path = entry.path();
        let is_dir = entry_path.is_dir();
        let is_file = entry_path.is_file();

        if !matches_mode(&entry_path, is_dir, is_file, mode) {
            continue;
        }

        let Some(name) = entry.file_name().to_str().map(ToOwned::to_owned) else {
            continue;
        };

        if !name.starts_with(&lookup.entry_prefix) {
            continue;
        }

        let mut replacement = format!("{}{}", lookup.raw_dir_prefix, name);
        if is_dir {
            replacement.push('/');
        }

        candidates.push(CompletionCandidate {
            replacement,
            is_dir,
            is_remote_hint: false,
        });
    }

    candidates.sort_by(|lhs, rhs| {
        let lhs_key = (!lhs.is_dir, lhs.replacement.to_lowercase());
        let rhs_key = (!rhs.is_dir, rhs.replacement.to_lowercase());
        lhs_key.cmp(&rhs_key)
    });

    Ok(CompletionSet {
        seed_input,
        candidates,
    })
}

pub fn resolve_user_path(input: &str) -> Result<PathBuf> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(anyhow::anyhow!("path is empty"));
    }

    let path = if trimmed == "~" || trimmed.starts_with("~/") {
        let home = env::var("HOME").context("HOME is not set")?;
        if trimmed == "~" {
            PathBuf::from(home)
        } else {
            PathBuf::from(home).join(trimmed.trim_start_matches("~/"))
        }
    } else {
        PathBuf::from(trimmed)
    };

    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(env::current_dir()
            .context("failed to determine current working directory")?
            .join(path))
    }
}

#[derive(Debug)]
struct Lookup {
    search_dir: PathBuf,
    raw_dir_prefix: String,
    entry_prefix: String,
}

impl Lookup {
    fn from_input(input: &str) -> Result<Self> {
        if input.is_empty() {
            let (search_dir, raw_dir_prefix) = match env::var("HOME") {
                Ok(home) => (PathBuf::from(home), "~/".to_string()),
                Err(_) => {
                    let cwd = env::current_dir()
                        .context("failed to determine current working directory")?;
                    let prefix = format!("{}/", cwd.display());
                    (cwd, prefix)
                }
            };

            return Ok(Self {
                search_dir,
                raw_dir_prefix,
                entry_prefix: String::new(),
            });
        }

        if input == "~" {
            return Ok(Self {
                search_dir: resolve_user_path("~")?,
                raw_dir_prefix: "~/".to_string(),
                entry_prefix: String::new(),
            });
        }

        let (raw_dir_prefix, entry_prefix) = split_input(input);
        let search_dir = if raw_dir_prefix.is_empty() {
            env::current_dir().context("failed to determine current working directory")?
        } else {
            resolve_user_path(&raw_dir_prefix)?
        };

        Ok(Self {
            search_dir,
            raw_dir_prefix,
            entry_prefix,
        })
    }
}

fn split_input(input: &str) -> (String, String) {
    if input.is_empty() {
        return (String::new(), String::new());
    }

    if input.ends_with('/') {
        return (input.to_string(), String::new());
    }

    match input.rfind('/') {
        Some(index) => (input[..=index].to_string(), input[index + 1..].to_string()),
        None => (String::new(), input.to_string()),
    }
}

fn matches_mode(path: &Path, is_dir: bool, is_file: bool, mode: PathCompletionMode) -> bool {
    if is_dir {
        return true;
    }

    if !is_file {
        return false;
    }

    match mode {
        PathCompletionMode::TorrentFile => path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("torrent")),
        PathCompletionMode::Directory => false,
    }
}
