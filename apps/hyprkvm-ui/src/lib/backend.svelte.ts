// The backend as the pages see it: connection state, the latest snapshot,
// and explicit actions. Every action goes through a Tauri command; pages
// never talk to the backend or the filesystem directly.

import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import type {
  AudioTarget,
  Connection,
  Controller,
  FieldError,
  NotificationSetup,
  PairingCode,
  PhonePairingUpdate,
  SettingsDraft,
  SettingsView,
  Snapshot,
  UiError,
  UiState
} from './types';

export const backend = $state<{
  connection: Connection;
  snapshot: Snapshot | null;
  offlineConfig: string | null;
  /** Bumped when the backend says it's restarting, so pages reload settings. */
  restarts: number;
}>({ connection: { status: 'connecting' }, snapshot: null, offlineConfig: null, restarts: 0 });

let started = false;

/** Subscribes once per page load. */
export async function connectBackend(): Promise<void> {
  if (started) return;
  started = true;
  await listen<Connection>('backend://connection', (event) => {
    const was = backend.connection.status;
    backend.connection = event.payload;
    if (event.payload.status === 'connected') {
      backend.offlineConfig = event.payload.hello.config_path;
      if (was !== 'connected') backend.restarts += 1;
    } else {
      backend.snapshot = null;
    }
  });
  await listen<Snapshot>('backend://snapshot', (event) => {
    backend.snapshot = event.payload;
  });
  const state = await invoke<UiState>('ui_state');
  backend.connection = state.connection;
  backend.snapshot = state.snapshot;
  backend.offlineConfig = state.offline_config;
}

export function onPhonePairing(handler: (update: PhonePairingUpdate) => void) {
  return listen<PhonePairingUpdate>('backend://phone-pairing', (event) => handler(event.payload));
}

/** Turns whatever a command threw into a UiError. */
export function asError(error: unknown): UiError {
  if (error && typeof error === 'object' && 'message' in error) return error as UiError;
  return { kind: 'failed', message: String(error) };
}

export const actions = {
  setInputForwarding: (enabled: boolean) => invoke<void>('set_input_forwarding', { enabled }),
  setController: (controller: Controller) => invoke<void>('set_controller', { controller }),
  setAudio: (target: AudioTarget) => invoke<void>('set_audio', { target }),
  disconnect: () => invoke<void>('disconnect_peer'),
  reconnect: () => invoke<void>('reconnect_peer'),
  armComputerPairing: () => invoke<void>('arm_computer_pairing'),
  startPhonePairing: () => invoke<PairingCode>('start_phone_pairing'),
  cancelPhonePairing: () => invoke<void>('cancel_phone_pairing'),
  unpairPhone: (name: string) => invoke<void>('unpair_phone', { name }),
  loadSettings: () => invoke<SettingsView>('load_settings'),
  validateSettings: (draft: SettingsDraft) => invoke<FieldError[]>('validate_settings', { draft }),
  saveSettings: (draft: SettingsDraft, base: SettingsDraft) =>
    invoke<{ restarting: boolean }>('save_settings', { draft, base }),
  getAutostart: () => invoke<boolean | null>('get_autostart'),
  setAutostart: (enabled: boolean) => invoke<void>('set_autostart', { enabled }),
  getNotifications: () => invoke<NotificationSetup | null>('get_notifications'),
  installNotificationBridge: (shellPath: string) =>
    invoke<string>('install_notification_bridge', { shellPath }),
  diagnostics: () => invoke<string>('diagnostics'),
  quitBackend: () => invoke<void>('quit_backend'),
  startBackend: () => invoke<string>('start_backend'),
  openLog: () => invoke<void>('open_log'),
  openPhonePairing: () => invoke<void>('open_phone_pairing')
};
