// Mirrors edge_control::protocol and the UI's own Tauri types. Keep in step
// with crates/edge-control/src/protocol.rs.

export type LinkState = 'starting' | 'listening' | 'connected' | 'paused' | 'error';
export type Controller = 'local' | 'peer';
export type AudioTarget =
  | { kind: 'off' }
  | { kind: 'to_peer' }
  | { kind: 'from_peer' }
  | { kind: 'to_phone'; fingerprint: string };

export interface Hello {
  protocol: number;
  backend_version: string;
  platform: string;
  pid: number;
  executable: string;
  config_path: string;
  state_dir: string;
  log_path: string;
  service_managed: boolean;
}

export interface PhoneState {
  fingerprint: string;
  name: string;
  connected: boolean;
  round_trip_ms: number | null;
  accepts_audio: boolean;
  /** Watching this computer through Remote View... */
  viewing: boolean;
  /** ...and controlling it with touch. */
  controlling: boolean;
}

export interface Snapshot {
  /** A u64 as a decimal string. */
  revision: string;
  state: LinkState;
  summary: string;
  local_name: string | null;
  peer_name: string | null;
  round_trip_ms: number | null;
  controller: Controller;
  can_switch_controller: boolean;
  switching_controller: boolean;
  input_forwarding: boolean;
  remote_input_active: boolean;
  audio: {
    target: AudioTarget;
    /** Fingerprint of a phone streaming its audio here. */
    from_phone: string | null;
    to_peer_available: boolean;
    from_peer_available: boolean;
  };
  pairing_armed: boolean;
  phones_supported: boolean;
  phones: PhoneState[];
  last_error: string | null;
  capabilities: {
    phones: boolean;
    notifications: boolean;
    autostart: boolean;
    listens: boolean;
    view_host: boolean;
  };
  diagnostics: { key: string; value: string }[];
}

export type Connection =
  | { status: 'connecting' }
  | { status: 'absent'; detail: string }
  | { status: 'starting' }
  | { status: 'incompatible'; server: number; ours: number }
  | { status: 'other_instance'; config_path: string; expected: string }
  | { status: 'connected'; hello: Hello }
  | { status: 'disconnected'; detail: string };

export interface UiState {
  connection: Connection;
  snapshot: Snapshot | null;
  offline_config: string | null;
}

export type PeerPosition = 'left' | 'right' | 'top' | 'bottom';
export type ClipboardMode = 'off' | 'text' | 'text_and_images';
export type GameCompatibility = 'compatible' | 'borderless' | 'always-enabled';
export type AudioRoute = 'disabled' | 'local-to-peer' | 'peer-to-local';

export interface NotificationConfig {
  enabled: boolean;
  coordinator_fingerprint: string;
  peers: string[];
}

export interface SettingsDraft {
  device_name: string;
  peer_host: string;
  port: string;
  position: PeerPosition;
  game_compatibility: GameCompatibility;
  clipboard: ClipboardMode;
  forward_phone_clipboard: boolean;
  send_clipboard_to_phones: boolean;
  audio_route: AudioRoute;
  audio_play_local: boolean;
  jitter_target_ms: number;
  notifications: NotificationConfig;
}

export interface SettingsView {
  draft: SettingsDraft;
  config_path: string;
  connector_only: boolean;
  local_address: string | null;
  audio_buffers_ms: number[];
}

export interface FieldError {
  field: string;
  message: string;
}

export interface UiError {
  kind: string;
  message: string;
  fields?: FieldError[];
}

export interface NotificationSetup {
  identity: string;
  computers: { name: string; fingerprint: string }[];
  status: string;
  active: boolean;
  has_store: boolean;
  coordinator: string;
  shell_path: string | null;
  socket_path: string;
  bridge_installed: boolean;
  bridge_connected: boolean;
}

export interface PairingCode {
  uri: string;
  expires_in_secs: number;
  qr_svg: string;
}

export type PhonePairingUpdate = { state: 'paired'; name: string } | { state: 'closed' };
