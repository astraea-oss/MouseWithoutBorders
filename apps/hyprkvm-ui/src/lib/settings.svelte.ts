// The settings draft, shared by Home (the Clipboard tile) and Settings so an
// unsaved edit survives moving between them and one Save bar covers both.

import { untrack } from 'svelte';
import { actions, asError, backend } from './backend.svelte';
import { note } from './feedback.svelte';
import type { FieldError, NotificationSetup, SettingsDraft, SettingsView } from './types';

class Settings {
  view = $state<SettingsView | null>(null);
  base = $state<SettingsDraft | null>(null);
  draft = $state<SettingsDraft | null>(null);
  loadError = $state('');
  errors = $state<FieldError[]>([]);
  saving = $state(false);

  /** Shared history setup; Save installs the bridge when turning it on. */
  notifications = $state<NotificationSetup | null>(null);
  notificationsLoaded = $state(false);
  shellPath = $state('');

  dirty = $derived(
    this.draft !== null && this.base !== null && JSON.stringify(this.draft) !== JSON.stringify(this.base)
  );

  #started = false;
  #reloadAfterSave = false;

  /** Starts loading and validating; pages call it, only the first call counts. */
  start() {
    if (this.#started) return;
    this.#started = true;
    $effect.root(() => {
      // Reload whenever the backend (re)connects, and once at start, but
      // never over unsaved edits unless they were just saved.
      $effect(() => {
        void backend.restarts;
        void backend.offlineConfig;
        if (untrack(() => this.dirty) && !this.#reloadAfterSave) return;
        this.#reloadAfterSave = false;
        this.load();
      });

      // Validate edits as they're made; the backend checks again on Save.
      $effect(() => {
        if (!this.dirty || !this.draft) {
          this.errors = [];
          return;
        }
        const snapshot = $state.snapshot(this.draft);
        const timer = setTimeout(async () => {
          try {
            this.errors = await actions.validateSettings(snapshot);
          } catch {
            // Offline without a known file; Save reports it.
          }
        }, 250);
        return () => clearTimeout(timer);
      });

      $effect(() => {
        const online = backend.connection.status === 'connected';
        void backend.restarts;
        if (!online) {
          this.notifications = null;
          this.notificationsLoaded = false;
          return;
        }
        untrack(() => this.refreshNotifications());
      });
    });
  }

  async load() {
    try {
      const view = await actions.loadSettings();
      this.view = view;
      this.base = structuredClone(view.draft);
      this.draft = structuredClone(view.draft);
      this.loadError = '';
      this.errors = [];
    } catch (error) {
      this.loadError = asError(error).message;
    }
  }

  async refreshNotifications() {
    try {
      this.notifications = await actions.getNotifications();
      if (this.notifications?.shell_path && !this.shellPath) this.shellPath = this.notifications.shell_path;
    } catch {
      this.notifications = null;
    }
    this.notificationsLoaded = true;
  }

  fieldError(field: string) {
    return this.errors.find((error) => error.field === field)?.message;
  }

  discard() {
    if (this.base) this.draft = structuredClone($state.snapshot(this.base));
  }

  async save() {
    const { draft, base, notifications } = this;
    if (!draft || !base) return;
    this.saving = true;
    try {
      // Turning shared history on installs the Quickshell bridge first, as
      // the old settings window did on Save.
      if (notifications && draft.notifications.enabled && !base.notifications.enabled && !notifications.bridge_installed) {
        if (!this.shellPath.trim()) throw { message: 'Choose your Quickshell shell.qml so the bridge can be installed' };
        await actions.installNotificationBridge(this.shellPath.trim());
      }
      const outcome = await actions.saveSettings($state.snapshot(draft), $state.snapshot(base));
      if (outcome.restarting) {
        this.#reloadAfterSave = true;
        note('Saved. Reconnecting…');
      } else {
        note('Saved for the next start');
        await this.load();
      }
    } catch (error) {
      const failure = asError(error);
      if (failure.fields?.length) this.errors = failure.fields;
      note(failure.message, true);
    } finally {
      this.saving = false;
    }
  }
}

export const settings = new Settings();
