use std::{
    io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

const AUDIO_ROUTE_STATE_VERSION: u16 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommittedAudioRoute {
    #[serde(default = "audio_route_state_version")]
    pub version: u16,
    /// Fingerprint of the machine whose system audio is captured. `None` is off.
    pub source_fingerprint: Option<String>,
}

fn audio_route_state_version() -> u16 {
    AUDIO_ROUTE_STATE_VERSION
}

impl CommittedAudioRoute {
    pub fn disabled() -> Self {
        Self {
            version: AUDIO_ROUTE_STATE_VERSION,
            source_fingerprint: None,
        }
    }

    pub fn from_source(source_fingerprint: impl Into<String>) -> Self {
        Self {
            version: AUDIO_ROUTE_STATE_VERSION,
            source_fingerprint: Some(source_fingerprint.into()),
        }
    }

    pub fn belongs_to(&self, first: &str, second: &str) -> bool {
        self.version == AUDIO_ROUTE_STATE_VERSION
            && self
                .source_fingerprint
                .as_deref()
                .is_none_or(|source| source == first || source == second)
    }
}

#[derive(Debug, Clone)]
pub struct AudioRouteStore {
    path: PathBuf,
}

impl AudioRouteStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub async fn load(&self) -> io::Result<Option<CommittedAudioRoute>> {
        let bytes = match tokio::fs::read(&self.path).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        toml::from_slice(&bytes)
            .map(Some)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }

    pub async fn save(&self, route: &CommittedAudioRoute) -> io::Result<()> {
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        tokio::fs::create_dir_all(parent).await?;
        let payload = toml::to_string_pretty(route)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let temporary = self.path.with_extension("toml.tmp");
        let mut file = tokio::fs::File::create(&temporary).await?;
        file.write_all(payload.as_bytes()).await?;
        file.sync_all().await?;
        drop(file);
        atomic_replace(&temporary, &self.path)?;
        Ok(())
    }
}

#[cfg(not(windows))]
fn atomic_replace(source: &Path, destination: &Path) -> io::Result<()> {
    std::fs::rename(source, destination)
}

#[cfg(windows)]
fn atomic_replace(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    // SAFETY: both owned buffers are NUL-terminated and live for the call.
    let moved = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn route_round_trips_and_rejects_another_pair() {
        let directory = tempfile::tempdir().unwrap();
        let store = AudioRouteStore::new(directory.path().join("state").join("audio.toml"));
        let route = CommittedAudioRoute::from_source("peer-a");
        store.save(&route).await.unwrap();
        assert_eq!(store.load().await.unwrap(), Some(route.clone()));
        assert!(route.belongs_to("local", "peer-a"));
        assert!(!route.belongs_to("local", "peer-b"));
    }
}
