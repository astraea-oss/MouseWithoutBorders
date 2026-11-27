<script lang="ts">
  import { goto } from '$app/navigation';
  import { Card, Label, Pill, Segment, StatusDot, Switch } from '@hyprkvm/hypr-ui';
  import { actions, backend } from '../../lib/backend.svelte';
  import { note, run, short, tail } from '../../lib/feedback.svelte';
  import Overlays from '../../lib/Overlays.svelte';
  import { settings } from '../../lib/settings.svelte';
  import type { GameCompatibility, PeerPosition } from '../../lib/types';

  settings.start();

  const view = $derived(settings.view);
  const draft = $derived(settings.draft);
  const notifications = $derived(settings.notifications);
  const notificationsLoaded = $derived(settings.notificationsLoaded);

  const snap = $derived(backend.snapshot);
  const conn = $derived(backend.connection);
  const online = $derived(conn.status === 'connected' && snap !== null);
  const peerShort = $derived(short(snap?.peer_name || draft?.peer_host || 'other PC', 10));

  // ---- Arrangement --------------------------------------------------------

  const offsets: Record<PeerPosition, [number, number]> = {
    right: [44, 0],
    left: [-44, 0],
    top: [0, -30],
    bottom: [0, 30]
  };
  const offset = $derived(offsets[draft?.position ?? 'left']);
  const edgeStyle = $derived(
    offset[1] === 0
      ? `width:3px;height:36px;left:calc(50% - 1.5px);top:calc(50% - 18px);transform:translate(${offset[0] / 2}px,0)`
      : `width:60px;height:3px;left:calc(50% - 30px);top:calc(50% - 1.5px);transform:translate(0,${offset[1] / 2}px)`
  );

  // ---- Phones -------------------------------------------------------------

  let confirmRemove = $state<string | null>(null);

  async function removePhone(name: string) {
    confirmRemove = null;
    await run(() => actions.unpairPhone(name), `Removed ${name}`);
  }

  // ---- Notifications -----------------------------------------------------

  function setSharedHistory(enabled: boolean) {
    if (!draft || !notifications) return;
    draft.notifications.enabled = enabled;
    if (enabled && draft.notifications.peers.length === 0) {
      // Start with the connected computer, as the old window did.
      const connected = notifications.computers.find((c) => c.name === snap?.peer_name);
      const first = connected ?? notifications.computers[0];
      if (first) draft.notifications.peers = [first.fingerprint];
      if (!draft.notifications.coordinator_fingerprint) {
        draft.notifications.coordinator_fingerprint = notifications.has_store
          ? notifications.coordinator
          : notifications.identity;
      }
    }
  }

  function togglePeer(fingerprint: string) {
    if (!draft) return;
    const peers = draft.notifications.peers;
    draft.notifications.peers = peers.includes(fingerprint)
      ? peers.filter((p) => p !== fingerprint)
      : [...peers, fingerprint];
  }

  function setOwner(fingerprint: string) {
    if (!draft || !notifications) return;
    draft.notifications.coordinator_fingerprint = fingerprint;
    if (fingerprint !== notifications.identity && !draft.notifications.peers.includes(fingerprint)) {
      draft.notifications.peers = [...draft.notifications.peers, fingerprint];
    }
  }

  async function reloadBridge() {
    await run(async () => {
      const message = await actions.installNotificationBridge(settings.shellPath.trim());
      note(message);
      await settings.refreshNotifications();
    });
  }

  // ---- This computer: Start at login is live, not part of the draft --------

  let autostart = $state<boolean | null>(null);

  $effect(() => {
    if (!online) {
      autostart = null;
      return;
    }
    void backend.restarts;
    actions.getAutostart().then((value) => (autostart = value)).catch(() => (autostart = null));
  });

  async function toggleAutostart(enabled: boolean) {
    await run(async () => {
      await actions.setAutostart(enabled);
      autostart = await actions.getAutostart();
    });
  }

  // ---- Advanced ------------------------------------------------------------

  async function copyReport() {
    await run(async () => {
      const report = await actions.diagnostics();
      await navigator.clipboard.writeText(report);
    }, 'Report copied');
  }

  const gameModes: { value: GameCompatibility; label: string }[] = [
    { value: 'always-enabled', label: 'Always enabled' },
    { value: 'borderless', label: 'Borderless games' },
    { value: 'compatible', label: 'Safe / compatible' }
  ];
</script>

<div class="app">
  <div class="top">
    <button class="ib" aria-label="Back" onclick={() => goto('/')}>
      <svg width="18" height="18" viewBox="0 0 24 24" style="transform:rotate(180deg)" aria-hidden="true">
        <path d="M4 12h15M13 6l6 6-6 6" stroke="currentColor" stroke-width="2.2" fill="none" stroke-linecap="square" />
      </svg>
    </button>
    <span class="ttl"><span>Settings</span></span>
  </div>
  <div class="hs" class:dirty={settings.dirty}>
    <div class="cols">
      <div class="col">
        {#if draft}
          <Card class="c">
            <Label>Arrangement</Label>
            <div class="arrmap" aria-hidden="true">
              <div class="scr me" style="transform:translate({-offset[0]}px,{-offset[1]}px)">
                {short(draft.device_name || 'This', 9)}
              </div>
              <div class="scr peer" style="transform:translate({offset[0]}px,{offset[1]}px)">{short(snap?.peer_name || draft.peer_host || 'Other', 9)}</div>
              <div class="edge" style={edgeStyle}></div>
            </div>
            <div class="r">
              <span>It sits to the</span>
              <span class="ctl">
                <Segment
                  label="Where the other computer sits"
                  value={draft.position}
                  options={[
                    { value: 'left', label: 'Left' },
                    { value: 'right', label: 'Right' },
                    { value: 'top', label: 'Top' },
                    { value: 'bottom', label: 'Bottom' }
                  ]}
                  onchange={(value) => draft && (draft.position = value)}
                />
              </span>
            </div>
          </Card>
        {/if}

        {#if snap?.phones_supported ?? true}
          <Card class="c">
            <Label>Phones</Label>
            {#each snap?.phones ?? [] as phone (phone.name)}
              <div class="r one">
                <span class="dev">
                  <StatusDot state={phone.connected ? 'on' : ''} />
                  <span class="grow">
                    {phone.name}
                    <small>{phone.connected ? `linked${phone.round_trip_ms != null ? ` · ${phone.round_trip_ms} ms` : ''}` : 'not connected'}</small>
                  </span>
                  {#if confirmRemove === phone.name}
                    <Pill small variant="danger" onclick={() => removePhone(phone.name)}>Remove</Pill>
                    <Pill small variant="ghost" onclick={() => (confirmRemove = null)}>Keep</Pill>
                  {:else}
                    <Pill small variant="ghost" onclick={() => (confirmRemove = phone.name)}>Remove…</Pill>
                  {/if}
                </span>
              </div>
            {:else}
              <div class="r one"><span class="hint">{online ? 'None paired yet' : 'Start the backend to see phones'}</span></div>
            {/each}
            {#if draft && draft.clipboard !== 'off' && (snap?.phones.length ?? 0) > 0}
              <div class="r">
                <span>Phone's copies</span>
                <span class="ctl">
                  <Switch
                    label="Pass the phone's clipboard on"
                    checked={draft.forward_phone_clipboard}
                    onchange={(value) => draft && (draft.forward_phone_clipboard = value)}
                  />
                  <span class="hint">pass on to {peerShort}</span>
                </span>
              </div>
              <div class="r">
                <span>This computer's copies</span>
                <span class="ctl">
                  <Switch
                    label="Send every copy to phones"
                    checked={draft.send_clipboard_to_phones}
                    onchange={(value) => draft && (draft.send_clipboard_to_phones = value)}
                  />
                  <span class="hint">{draft.send_clipboard_to_phones ? 'every copy goes to phones' : 'only when you press send'}</span>
                </span>
              </div>
            {/if}
            <div class="r one"><span><Pill small disabled={!online} onclick={() => run(actions.openPhonePairing)}>+ Add phone</Pill></span></div>
          </Card>
        {/if}
      </div>

      <div class="col">
        {#if draft && view && !view.connector_only}
          <Card class="c">
            <Label>Notifications <span>{notifications?.status ?? ''}</span></Label>
            {#if !online}
              <div class="r"><span>Shared history</span><span class="ctl"><span class="hint">Start the backend to change notification sharing.</span></span></div>
            {:else if notificationsLoaded && !notifications}
              <div class="r"><span>Shared history</span><span class="ctl"><span class="hint">Not available on this computer.</span></span></div>
            {:else if notifications}
              <div class="r">
                <span>Shared history</span>
                <span class="ctl">
                  <Switch label="Shared history" checked={draft.notifications.enabled} onchange={setSharedHistory} />
                  {#if settings.fieldError('notifications')}<span class="err">{settings.fieldError('notifications')}</span>{/if}
                </span>
              </div>
              {#if draft.notifications.enabled}
                <div class="r">
                  <span>Share with</span>
                  <span class="ctl">
                    {#each notifications.computers as computer (computer.fingerprint)}
                      <Pill
                        small
                        variant={draft.notifications.peers.includes(computer.fingerprint) ? '' : 'ghost'}
                        aria-pressed={draft.notifications.peers.includes(computer.fingerprint)}
                        onclick={() => togglePeer(computer.fingerprint)}
                      >{computer.name}</Pill>
                    {:else}
                      <span class="hint">Pair a computer first.</span>
                    {/each}
                  </span>
                </div>
                <div class="r">
                  <span>History owner</span>
                  <span class="ctl">
                    <select
                      class="field"
                      aria-label="History owner"
                      disabled={notifications.has_store}
                      value={draft.notifications.coordinator_fingerprint}
                      onchange={(event) => setOwner(event.currentTarget.value)}
                    >
                      <option value="" disabled>Choose a computer</option>
                      <option value={notifications.identity}>{draft.device_name} (this computer)</option>
                      {#each notifications.computers as computer (computer.fingerprint)}
                        <option value={computer.fingerprint}>{computer.name}</option>
                      {/each}
                    </select>
                    {#if notifications.has_store}<span class="hint">Fixed once history exists.</span>{/if}
                  </span>
                </div>
              {/if}
              <div class="r">
                <span>Quickshell</span>
                <span class="ctl">
                  {#if notifications.shell_path}
                    <span class="hint" style="flex:1">
                      {notifications.bridge_installed && notifications.active && !notifications.bridge_connected
                        ? 'Installed · not connected'
                        : notifications.bridge_installed
                          ? 'Bridge installed'
                          : 'Bridge installs on Save'}
                    </span>
                  {:else}
                    <input class="field" style="flex:1" placeholder="Path to shell.qml" aria-label="Quickshell shell.qml" bind:value={settings.shellPath} />
                  {/if}
                  {#if notifications.bridge_installed}
                    <Pill small variant="ghost" onclick={reloadBridge}>Reload</Pill>
                  {/if}
                </span>
              </div>
            {/if}
          </Card>
        {/if}

        {#if draft && view}
          <Card class="c">
            <Label>This computer</Label>
            <div class="r">
              <span>Name</span>
              <span class="ctl">
                <input class="field" class:bad={settings.fieldError('device_name')} aria-label="Name" bind:value={draft.device_name} />
                {#if settings.fieldError('device_name')}<span class="err">{settings.fieldError('device_name')}</span>{/if}
              </span>
            </div>
            <div class="r">
              <span>Other computer</span>
              <span class="ctl">
                <input
                  class="field"
                  class:bad={settings.fieldError('peer_host')}
                  aria-label="Other computer's address"
                  placeholder={view.connector_only ? 'Address' : 'Address (optional: it can dial you)'}
                  bind:value={draft.peer_host}
                />
                {#if settings.fieldError('peer_host')}<span class="err">{settings.fieldError('peer_host')}</span>{/if}
              </span>
            </div>
            <div class="r">
              <span>Port</span>
              <span class="ctl">
                <input class="field" class:bad={settings.fieldError('port')} style="max-width:110px" inputmode="numeric" aria-label="Port" bind:value={draft.port} />
                {#if settings.fieldError('port')}<span class="err">{settings.fieldError('port')}</span>{/if}
              </span>
            </div>
            <div class="r">
              <span>Start at login</span>
              <span class="ctl">
                <Switch label="Start at login" checked={autostart ?? false} disabled={autostart === null} onchange={toggleAutostart} />
                {#if autostart === null}<span class="hint">{online ? 'Not installed as a service' : 'Start the backend to change'}</span>{/if}
              </span>
            </div>
          </Card>

          <Card class="c">
            <Label>Advanced</Label>
            <div class="r">
              <span>Audio buffer</span>
              <span class="ctl">
                <select
                  class="field"
                  style="max-width:140px"
                  aria-label="Audio buffer"
                  value={draft.jitter_target_ms}
                  onchange={(event) => draft && (draft.jitter_target_ms = Number(event.currentTarget.value))}
                >
                  {#each view.audio_buffers_ms as ms (ms)}<option value={ms}>{ms} ms</option>{/each}
                </select>
              </span>
            </div>
            <div class="r">
              <span>Play here too</span>
              <span class="ctl">
                <Switch
                  label="Play here too"
                  checked={draft.audio_play_local}
                  onchange={(value) => draft && (draft.audio_play_local = value)}
                />
                <span class="hint">when sending audio to {peerShort}</span>
              </span>
            </div>
            <div class="r">
              <span>Games</span>
              <span class="ctl">
                <select
                  class="field"
                  aria-label="Game compatibility"
                  value={draft.game_compatibility}
                  onchange={(event) => draft && (draft.game_compatibility = event.currentTarget.value as GameCompatibility)}
                >
                  {#each gameModes as mode (mode.value)}<option value={mode.value}>{mode.label}</option>{/each}
                </select>
              </span>
            </div>
            <div class="r">
              <span>Diagnostics</span>
              <span class="ctl">
                <Pill small variant="ghost" onclick={copyReport}>Copy report</Pill>
                <Pill small variant="ghost" disabled={!online} onclick={() => run(actions.openLog)}>Open log</Pill>
              </span>
            </div>
            <div class="r">
              <span>Backend</span>
              <span class="ctl">
                {#if online}
                  <Pill small variant="danger" onclick={() => run(actions.quitBackend, 'Stopping the backend')}>Quit backend</Pill>
                {:else if conn.status === 'absent'}
                  <Pill small variant="ghost" onclick={() => run(actions.startBackend, 'Starting the backend…')}>Start backend</Pill>
                {/if}
                <span class="hint">{tail(view.config_path, 44)}</span>
              </span>
            </div>
          </Card>
        {:else if settings.loadError}
          <Card class="c">
            <Label>Settings</Label>
            <div class="r"><span>Unavailable</span><span class="ctl"><span class="hint">{settings.loadError}</span></span></div>
          </Card>
        {/if}
      </div>
    </div>
  </div>
  <Overlays />
</div>
