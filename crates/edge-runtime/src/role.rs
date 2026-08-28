use std::{
    io,
    path::{Path, PathBuf},
};

use edge_protocol::{Edge, RoleState, RoleTransitionState};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

const ROLE_STATE_VERSION: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputDirectionCapabilities {
    pub controller_can_capture: bool,
    pub receiver_can_inject: bool,
}

impl InputDirectionCapabilities {
    pub fn complete(self) -> bool {
        self.controller_can_capture && self.receiver_can_inject
    }
}

pub fn select_initial_controller<'a>(
    connector_fingerprint: &'a str,
    listener_fingerprint: &'a str,
    prefer_connector: bool,
    connector_controls: InputDirectionCapabilities,
    listener_controls: InputDirectionCapabilities,
) -> Option<&'a str> {
    let preferred = if prefer_connector {
        (connector_fingerprint, connector_controls)
    } else {
        (listener_fingerprint, listener_controls)
    };
    let fallback = if prefer_connector {
        (listener_fingerprint, listener_controls)
    } else {
        (connector_fingerprint, connector_controls)
    };
    preferred
        .1
        .complete()
        .then_some(preferred.0)
        .or_else(|| fallback.1.complete().then_some(fallback.0))
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RoleStateError {
    #[error("controller identity is not part of this authenticated pair")]
    UnknownController,
    #[error("a role transition is already in progress")]
    TransitionInProgress,
    #[error("role epoch overflow")]
    EpochOverflow,
    #[error("role message has stale or unexpected epoch {actual}; expected {expected}")]
    UnexpectedEpoch { expected: u64, actual: u64 },
    #[error("role message does not match the prepared controller")]
    ControllerMismatch,
    #[error("role message has an invalid transition state")]
    InvalidTransition,
    #[error("no role transition is in progress")]
    NoTransition,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoleDecision {
    Commit(RoleState),
    Abort(RoleState),
}

#[derive(Debug, Clone)]
struct PendingRole {
    previous: RoleState,
    proposed_controller_fingerprint: String,
    proposed_epoch: u64,
}

/// Connector-owned serialization for a two-node role assignment.
#[derive(Debug, Clone)]
pub struct RoleCoordinator {
    connector_fingerprint: String,
    listener_fingerprint: String,
    stable: RoleState,
    pending: Option<PendingRole>,
}

impl RoleCoordinator {
    pub fn new(
        connector_fingerprint: String,
        listener_fingerprint: String,
        controller_fingerprint: Option<String>,
        role_epoch: u64,
        listener_position: Edge,
        paused: bool,
    ) -> Result<Self, RoleStateError> {
        if controller_fingerprint.as_deref().is_some_and(|controller| {
            controller != connector_fingerprint && controller != listener_fingerprint
        }) {
            return Err(RoleStateError::UnknownController);
        }
        Ok(Self {
            connector_fingerprint,
            listener_fingerprint,
            stable: RoleState {
                controller_fingerprint,
                role_epoch,
                transition: RoleTransitionState::Stable,
                listener_position,
                paused,
                failure_detail: None,
            },
            pending: None,
        })
    }

    pub fn state(&self) -> &RoleState {
        &self.stable
    }

    pub fn is_transitioning(&self) -> bool {
        self.pending.is_some()
    }

    pub fn set_paused(&mut self, paused: bool) -> Result<RoleState, RoleStateError> {
        if self.pending.is_some() {
            return Err(RoleStateError::TransitionInProgress);
        }
        self.stable.paused = paused;
        Ok(self.stable.clone())
    }

    pub fn prepare(&mut self, controller_fingerprint: &str) -> Result<RoleState, RoleStateError> {
        if self.pending.is_some() {
            return Err(RoleStateError::TransitionInProgress);
        }
        if controller_fingerprint != self.connector_fingerprint
            && controller_fingerprint != self.listener_fingerprint
        {
            return Err(RoleStateError::UnknownController);
        }
        let proposed_epoch = self
            .stable
            .role_epoch
            .checked_add(1)
            .ok_or(RoleStateError::EpochOverflow)?;
        let prepare = RoleState {
            controller_fingerprint: Some(controller_fingerprint.to_string()),
            role_epoch: proposed_epoch,
            transition: RoleTransitionState::Preparing {
                proposed_controller_fingerprint: controller_fingerprint.to_string(),
            },
            listener_position: self.stable.listener_position,
            paused: self.stable.paused,
            failure_detail: None,
        };
        self.pending = Some(PendingRole {
            previous: self.stable.clone(),
            proposed_controller_fingerprint: controller_fingerprint.to_string(),
            proposed_epoch,
        });
        Ok(prepare)
    }

    pub fn finish_ready(
        &mut self,
        role_epoch: u64,
        local_capture_ready: bool,
        local_inject_ready: bool,
        remote_capture_ready: bool,
        remote_inject_ready: bool,
        failure_detail: Option<String>,
    ) -> Result<RoleDecision, RoleStateError> {
        let pending = self.pending.take().ok_or(RoleStateError::NoTransition)?;
        if role_epoch != pending.proposed_epoch {
            let expected = pending.proposed_epoch;
            self.pending = Some(pending);
            return Err(RoleStateError::UnexpectedEpoch {
                expected,
                actual: role_epoch,
            });
        }
        if local_capture_ready && local_inject_ready && remote_capture_ready && remote_inject_ready
        {
            self.stable = RoleState {
                controller_fingerprint: Some(pending.proposed_controller_fingerprint),
                role_epoch: pending.proposed_epoch,
                transition: RoleTransitionState::Stable,
                listener_position: pending.previous.listener_position,
                paused: pending.previous.paused,
                failure_detail: None,
            };
            Ok(RoleDecision::Commit(self.stable.clone()))
        } else {
            self.stable = pending.previous;
            let mut abort = self.stable.clone();
            abort.transition = RoleTransitionState::Failed;
            abort.failure_detail = failure_detail
                .or_else(|| Some("one or more input backends were not ready".to_string()));
            Ok(RoleDecision::Abort(abort))
        }
    }

    pub fn abort(&mut self, detail: impl Into<String>) -> Result<RoleState, RoleStateError> {
        let pending = self.pending.take().ok_or(RoleStateError::NoTransition)?;
        self.stable = pending.previous;
        let mut abort = self.stable.clone();
        abort.transition = RoleTransitionState::Failed;
        abort.failure_detail = Some(detail.into());
        Ok(abort)
    }
}

pub fn validate_prepare(
    current: &RoleState,
    prepare: &RoleState,
    connector_fingerprint: &str,
    listener_fingerprint: &str,
) -> Result<(), RoleStateError> {
    let proposed = match &prepare.transition {
        RoleTransitionState::Preparing {
            proposed_controller_fingerprint,
        } => proposed_controller_fingerprint,
        _ => return Err(RoleStateError::InvalidTransition),
    };
    if proposed != connector_fingerprint && proposed != listener_fingerprint {
        return Err(RoleStateError::UnknownController);
    }
    if prepare.controller_fingerprint.as_deref() != Some(proposed) {
        return Err(RoleStateError::ControllerMismatch);
    }
    let expected = current
        .role_epoch
        .checked_add(1)
        .ok_or(RoleStateError::EpochOverflow)?;
    if prepare.role_epoch != expected {
        return Err(RoleStateError::UnexpectedEpoch {
            expected,
            actual: prepare.role_epoch,
        });
    }
    Ok(())
}

pub fn validate_commit(prepared: &RoleState, commit: &RoleState) -> Result<(), RoleStateError> {
    if !matches!(prepared.transition, RoleTransitionState::Preparing { .. })
        || !matches!(commit.transition, RoleTransitionState::Stable)
    {
        return Err(RoleStateError::InvalidTransition);
    }
    if prepared.role_epoch != commit.role_epoch {
        return Err(RoleStateError::UnexpectedEpoch {
            expected: prepared.role_epoch,
            actual: commit.role_epoch,
        });
    }
    if prepared.controller_fingerprint != commit.controller_fingerprint {
        return Err(RoleStateError::ControllerMismatch);
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommittedRole {
    #[serde(default = "role_state_version")]
    pub version: u16,
    pub controller_fingerprint: String,
}

fn role_state_version() -> u16 {
    ROLE_STATE_VERSION
}

impl CommittedRole {
    pub fn new(controller_fingerprint: impl Into<String>) -> Self {
        Self {
            version: ROLE_STATE_VERSION,
            controller_fingerprint: controller_fingerprint.into(),
        }
    }

    pub fn belongs_to(&self, first: &str, second: &str) -> bool {
        self.version == ROLE_STATE_VERSION
            && (self.controller_fingerprint == first || self.controller_fingerprint == second)
    }
}

#[derive(Debug, Clone)]
pub struct RoleStore {
    path: PathBuf,
}

impl RoleStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub async fn load(&self) -> io::Result<Option<CommittedRole>> {
        let bytes = match tokio::fs::read(&self.path).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        toml::from_slice(&bytes)
            .map(Some)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }

    pub async fn save(&self, role: &CommittedRole) -> io::Result<()> {
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        tokio::fs::create_dir_all(parent).await?;
        let payload = toml::to_string_pretty(role)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let temporary = self.path.with_extension("toml.tmp");
        let mut file = tokio::fs::File::create(&temporary).await?;
        file.write_all(payload.as_bytes()).await?;
        file.sync_all().await?;
        drop(file);
        atomic_replace(&temporary, &self.path)?;
        Ok(())
    }

    pub async fn clear(&self) -> io::Result<()> {
        match tokio::fs::remove_file(&self.path).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
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
    // SAFETY: both paths are owned, NUL-terminated UTF-16 buffers that remain
    // alive for the duration of the call.
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

    fn coordinator() -> RoleCoordinator {
        RoleCoordinator::new(
            "connector".to_string(),
            "listener".to_string(),
            Some("connector".to_string()),
            1,
            Edge::Left,
            false,
        )
        .unwrap()
    }

    #[test]
    fn initial_selection_uses_preference_then_complete_fallback() {
        let complete = InputDirectionCapabilities {
            controller_can_capture: true,
            receiver_can_inject: true,
        };
        let incomplete = InputDirectionCapabilities {
            controller_can_capture: true,
            receiver_can_inject: false,
        };
        assert_eq!(
            select_initial_controller("connector", "listener", true, incomplete, complete),
            Some("listener")
        );
        assert_eq!(
            select_initial_controller("connector", "listener", false, incomplete, incomplete),
            None
        );
    }

    #[test]
    fn connector_serializes_simultaneous_requests() {
        let mut coordinator = coordinator();
        let prepare = coordinator.prepare("listener").unwrap();
        assert_eq!(prepare.role_epoch, 2);
        assert_eq!(
            coordinator.prepare("connector"),
            Err(RoleStateError::TransitionInProgress)
        );
    }

    #[test]
    fn readiness_commits_only_the_prepared_epoch() {
        let mut coordinator = coordinator();
        coordinator.prepare("listener").unwrap();
        assert!(matches!(
            coordinator.finish_ready(99, true, true, true, true, None),
            Err(RoleStateError::UnexpectedEpoch { .. })
        ));
        let RoleDecision::Commit(commit) = coordinator
            .finish_ready(2, true, true, true, true, None)
            .unwrap()
        else {
            panic!("expected commit");
        };
        assert_eq!(commit.controller_fingerprint.as_deref(), Some("listener"));
        assert_eq!(commit.role_epoch, 2);
    }

    #[test]
    fn failed_preflight_aborts_without_advancing_the_epoch() {
        let mut coordinator = coordinator();
        coordinator.prepare("listener").unwrap();
        let RoleDecision::Abort(abort) = coordinator
            .finish_ready(
                2,
                true,
                true,
                false,
                true,
                Some("capture denied".to_string()),
            )
            .unwrap()
        else {
            panic!("expected abort");
        };
        assert_eq!(coordinator.state().role_epoch, 1);
        assert_eq!(abort.failure_detail.as_deref(), Some("capture denied"));
    }

    #[test]
    fn every_readiness_failure_aborts_the_handover() {
        for failed_index in 0..4 {
            let mut coordinator = coordinator();
            coordinator.prepare("listener").unwrap();
            let mut ready = [true; 4];
            ready[failed_index] = false;
            let decision = coordinator
                .finish_ready(2, ready[0], ready[1], ready[2], ready[3], None)
                .unwrap();
            assert!(matches!(decision, RoleDecision::Abort(_)));
            assert_eq!(coordinator.state().role_epoch, 1);
            assert_eq!(
                coordinator.state().controller_fingerprint.as_deref(),
                Some("connector")
            );
        }
    }

    #[test]
    fn pause_changes_only_stable_session_state() {
        let mut coordinator = coordinator();
        assert!(coordinator.set_paused(true).unwrap().paused);
        coordinator.prepare("listener").unwrap();
        assert_eq!(
            coordinator.set_paused(false),
            Err(RoleStateError::TransitionInProgress)
        );
    }

    #[test]
    fn participant_rejects_stale_prepare_and_mismatched_commit() {
        let coordinator = coordinator();
        let mut stale = coordinator.state().clone();
        stale.transition = RoleTransitionState::Preparing {
            proposed_controller_fingerprint: "listener".to_string(),
        };
        stale.controller_fingerprint = Some("listener".to_string());
        assert!(matches!(
            validate_prepare(coordinator.state(), &stale, "connector", "listener"),
            Err(RoleStateError::UnexpectedEpoch { .. })
        ));

        stale.role_epoch = 2;
        validate_prepare(coordinator.state(), &stale, "connector", "listener").unwrap();
        let mut commit = stale.clone();
        commit.transition = RoleTransitionState::Stable;
        commit.controller_fingerprint = Some("connector".to_string());
        assert_eq!(
            validate_commit(&stale, &commit),
            Err(RoleStateError::ControllerMismatch)
        );
    }

    #[tokio::test]
    async fn committed_role_is_atomically_replaced() {
        let directory = tempfile::tempdir().unwrap();
        let store = RoleStore::new(directory.path().join("state").join("role.toml"));
        store.save(&CommittedRole::new("first")).await.unwrap();
        store.save(&CommittedRole::new("second")).await.unwrap();
        assert_eq!(
            store.load().await.unwrap(),
            Some(CommittedRole::new("second"))
        );
        assert!(!store.path().with_extension("toml.tmp").exists());
    }
}
