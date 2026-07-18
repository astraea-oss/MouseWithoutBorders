use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorHandoffState {
    Disabled,
    Local,
    RemotePending { hide_at: Instant },
    RemoteHidden,
}

impl CursorHandoffState {
    pub fn ready() -> Self {
        Self::Local
    }

    pub fn enter_remote(&mut self, now: Instant, hide_delay: Duration) {
        if !matches!(self, Self::Disabled) {
            *self = Self::RemotePending {
                hide_at: now + hide_delay,
            };
        }
    }

    pub fn hide_at(self) -> Option<Instant> {
        match self {
            Self::RemotePending { hide_at } => Some(hide_at),
            _ => None,
        }
    }

    pub fn mark_hidden_if_due(&mut self, now: Instant) -> bool {
        let Self::RemotePending { hide_at } = *self else {
            return false;
        };
        if now < hide_at {
            return false;
        }
        *self = Self::RemoteHidden;
        true
    }

    pub fn local_activity(&mut self) -> bool {
        if !self.remote_active() {
            return false;
        }
        *self = Self::Local;
        true
    }

    pub fn release_remote(&mut self) -> bool {
        let was_hidden = matches!(self, Self::RemoteHidden);
        if !matches!(self, Self::Disabled) {
            *self = Self::Local;
        }
        was_hidden
    }

    pub fn disable(&mut self) -> bool {
        let was_hidden = matches!(self, Self::RemoteHidden);
        *self = Self::Disabled;
        was_hidden
    }

    pub fn enabled(self) -> bool {
        !matches!(self, Self::Disabled)
    }

    pub fn remote_active(self) -> bool {
        matches!(self, Self::RemotePending { .. } | Self::RemoteHidden)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hides_only_after_delay_and_releases_on_local_activity() {
        let now = Instant::now();
        let mut state = CursorHandoffState::ready();
        state.enter_remote(now, Duration::from_millis(100));

        assert!(!state.mark_hidden_if_due(now + Duration::from_millis(99)));
        assert!(state.mark_hidden_if_due(now + Duration::from_millis(100)));
        assert_eq!(state, CursorHandoffState::RemoteHidden);
        assert!(state.local_activity());
        assert_eq!(state, CursorHandoffState::Local);
        assert!(!state.local_activity());
    }

    #[test]
    fn release_cancels_pending_hide() {
        let now = Instant::now();
        let mut state = CursorHandoffState::ready();
        state.enter_remote(now, Duration::from_millis(100));

        assert!(!state.release_remote());
        assert_eq!(state.hide_at(), None);
        assert!(!state.mark_hidden_if_due(now + Duration::from_secs(1)));
    }

    #[test]
    fn disabled_state_never_activates() {
        let now = Instant::now();
        let mut state = CursorHandoffState::Disabled;
        state.enter_remote(now, Duration::ZERO);

        assert!(!state.enabled());
        assert!(!state.remote_active());
        assert!(!state.local_activity());
    }
}
