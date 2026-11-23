<script lang="ts">
  import { untrack } from 'svelte';
  import { Card, Label, Pill, Segment, StatusDot, Switch, Tile } from '@hyprkvm/hypr-ui';
  import { actions, asError, backend } from '../lib/backend.svelte';
  import type {
    AudioTarget,
    ClipboardMode,
    FieldError,
    GameCompatibility,
    NotificationSetup,
    PeerPosition,
    SettingsDraft,
    SettingsView
  } from '../lib/types';

  // ---- Backend state ------------------------------------------------------

  let settings = $state<SettingsView | null>(null);
  let base = $state<SettingsDraft | null>(null);
  let draft = $state<SettingsDraft | null>(null);

  const snap = $derived(backend.snapshot);
  const conn = $derived(backend.connection);
  const online = $derived(conn.status === 'connected' && snap !== null);
  const linked = $derived(online && snap?.state === 'connected');
  const peer = $derived(snap?.peer_name || settings?.draft.peer_host || 'the other computer');
  const peerShort = $derived(short(snap?.peer_name || settings?.draft.peer_host || 'other PC', 10));

  function short(text: string, max: number) {
    return text.length > max ? text.slice(0, max - 1) + '…' : text;
  }

  /** Keeps the end of a path, which is the part that tells files apart. */
  function tail(text: string, max: number) {
    return text.length > max ? '…' + text.slice(text.length - max + 1) : text;
  }

  const hero = $derived.by(() => {
    switch (conn.status) {
      case 'connected':
        if (!snap) return { word: '…', dot: '' as const };
        return {
          connected: { word: 'Linked', dot: 'live' as const },
          listening: snap.pairing_armed
            ? { word: 'Pairing', dot: 'blink' as const }
            : { word: 'Waiting', dot: 'blink' as const },
          paused: { word: 'Paused', dot: '' as const },
          error: { word: 'Error', dot: '' as const },
          starting: { word: 'Starting', dot: 'blink' as const }
        }[snap.state];
      case 'absent':
        return { word: 'Offline', dot: '' as const };
      case 'starting':
      case 'connecting':
        return { word: 'Starting', dot: 'blink' as const };
      case 'disconnected':
        return { word: 'Lost', dot: 'blink' as const };
      case 'incompatible':
        return { word: 'Update', dot: '' as const };
      case 'other_instance':
        return { word: 'Elsewhere', dot: '' as const };
    }
  });

  const heroLine = $derived.by(() => {
    switch (conn.status) {
      case 'connected':
        return snap?.summary ?? '';
      case 'absent':
        return "hyprKvm's backend isn't running. Settings below still save for its next start.";
      case 'starting':
      case 'connecting':
        return 'Connecting to the backend…';
      case 'disconnected':
        return 'Lost the backend. Reconnecting…';
      case 'incompatible':
        return conn.server === 0
          ? 'The running backend is older than this window and has no control protocol. Update and restart it.'
          : `The backend speaks control protocol ${conn.server}; this window speaks ${conn.ours}. Update both together.`;
      case 'other_instance':
        return `A backend is running for ${conn.config_path}, not ${conn.expected}. Stop it or open hyprKvm from its own folder.`;
    }
  });

  // ---- Toast ---------------------------------------------------------------

  let toast = $state({ text: '', bad: false, show: false });
  let toastTimer: ReturnType<typeof setTimeout> | undefined;
  function note(text: string, bad = false) {
    toast = { text, bad, show: true };
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => (toast.show = false), bad ? 4000 : 1800);
  }

  /** Runs a live action and reports a failure without throwing. */
  async function run(action: () => Promise<unknown>, done?: string) {
    try {
      await action();
      if (done) note(done);
    } catch (error) {
      note(asError(error).message, true);
    }
  }

  // ---- Settings draft -----------------------------------------------------

  let settingsError = $state('');
  let errors = $state<FieldError[]>([]);
  let saving = $state(false);

  const dirty = $derived(draft !== null && base !== null && JSON.stringify(draft) !== JSON.stringify(base));

  async function loadSettings() {
    try {
      const view = await actions.loadSettings();
      settings = view;
      base = structuredClone(view.draft);
      draft = structuredClone(view.draft);
      settingsError = '';
      errors = [];
    } catch (error) {
      settingsError = asError(error).message;
    }
  }

  // Reload whenever the backend (re)connects, and once at start, but never
  // over unsaved edits unless they were just saved.
  let reloadAfterSave = false;
  $effect(() => {
    void backend.restarts;
    void backend.offlineConfig;
    if (untrack(() => dirty) && !reloadAfterSave) return;
    reloadAfterSave = false;
    loadSettings();
  });

  // Validate edits as they're made; the backend checks again on Save.
  $effect(() => {
    if (!dirty || !draft) {
      errors = [];
      return;
    }
    const snapshot = $state.snapshot(draft);
    const timer = setTimeout(async () => {
      try {
        errors = await actions.validateSettings(snapshot);
      } catch {
        // Offline without a known file; Save reports it.
      }
    }, 250);
    return () => clearTimeout(timer);
  });

  function fieldError(field: string) {
    return errors.find((error) => error.field === field)?.message;
  }

  function discard() {
    if (base) draft = structuredClone($state.snapshot(base));
  }

  async function save() {
    if (!draft || !base) return;
    saving = true;
    try {
      // Turning shared history on installs the Quickshell bridge first, as
      // the old settings window did on Save.
      if (
        notifications &&
        draft.notifications.enabled &&
        !base.notifications.enabled &&
        !notifications.bridge_installed
      ) {
        if (!shellPath.trim()) throw { message: 'Choose your Quickshell shell.qml so the bridge can be installed' };
        await actions.installNotificationBridge(shellPath.trim());
      }
      const outcome = await actions.saveSettings($state.snapshot(draft), $state.snapshot(base));
      if (outcome.restarting) {
        reloadAfterSave = true;
        note('Saved. Restarting hyprKvm…');
      } else {
        note('Saved for the next start');
        await loadSettings();
      }
    } catch (error) {
      const failure = asError(error);
      if (failure.fields?.length) errors = failure.fields;
      note(failure.message, true);
    } finally {
      saving = false;
    }
  }

  // ---- Live extras: autostart and notifications -------------------------

  let autostart = $state<boolean | null>(null);
  let notifications = $state<NotificationSetup | null>(null);
  let notificationsLoaded = $state(false);
  let shellPath = $state('');

  $effect(() => {
    if (!online) {
      autostart = null;
      notifications = null;
      notificationsLoaded = false;
      return;
    }
    void backend.restarts;
    actions.getAutostart().then((value) => (autostart = value)).catch(() => (autostart = null));
    refreshNotifications();
  });

  async function refreshNotifications() {
    try {
      notifications = await actions.getNotifications();
      if (notifications?.shell_path && !shellPath) shellPath = notifications.shell_path;
    } catch {
      notifications = null;
    }
    notificationsLoaded = true;
  }

  async function toggleAutostart(enabled: boolean) {
    await run(async () => {
      await actions.setAutostart(enabled);
      autostart = await actions.getAutostart();
    });
  }

  // ---- Tiles ----------------------------------------------------------------

  const controlTile = $derived.by(() => {
    if (!linked || !snap) return { word: '—', foot: 'not connected', dot: '' as const };
    const here = snap.controller === 'local';
    if (snap.switching_controller) return { word: '…', foot: 'switching', dot: 'blink' as const };
    return here
      ? { word: 'Here', foot: `you drive ${peerShort}`, dot: 'live' as const }
      : { word: peerShort, foot: `${peerShort} drives you`, dot: 'on' as const };
  });

  function flipControl() {
    if (!snap?.can_switch_controller) return;
    run(() => actions.setController(snap.controller === 'local' ? 'peer' : 'local'));
  }

  const clipboardWords: Record<ClipboardMode, [string, string]> = {
    off: ['Off', 'not shared'],
    text: ['Text', 'text only'],
    text_and_images: ['+Img', 'text and images']
  };
  const clipboardOrder: ClipboardMode[] = ['off', 'text', 'text_and_images'];

  function cycleClipboard() {
    if (!draft) return;
    const next = clipboardOrder[(clipboardOrder.indexOf(draft.clipboard) + 1) % clipboardOrder.length];
    draft.clipboard = next;
  }

  const audioKind = $derived(snap?.audio.target.kind ?? 'off');
  const audioTile = $derived.by(() => {
    if (!snap) return { word: 'Off', foot: 'not connected', dot: '' as const };
    if (snap.audio.from_phone) return { word: 'Phone', foot: 'from a phone', dot: 'live' as const };
    const target = snap.audio.target;
    switch (target.kind) {
      case 'to_peer':
        return { word: 'Send', foot: `to ${peerShort}`, dot: 'live' as const };
      case 'from_peer':
        return { word: 'Play', foot: `from ${peerShort}`, dot: 'live' as const };
      case 'to_phone': {
        const phone = snap.phones.find((p) => p.fingerprint === target.fingerprint);
        return { word: 'Send', foot: `to ${short(phone?.name ?? 'phone', 12)}`, dot: 'live' as const };
      }
      default:
        return { word: 'Off', foot: 'no audio', dot: '' as const };
    }
  });

  const audioChoices = $derived.by(() => {
    const choices: { value: string; label: string; disabled?: boolean }[] = [
      { value: 'off', label: 'Off' },
      { value: 'to_peer', label: 'Send', disabled: !snap?.audio.to_peer_available },
      { value: 'from_peer', label: 'Play', disabled: !snap?.audio.from_peer_available }
    ];
    return choices;
  });

  function setAudio(kind: string) {
    const target: AudioTarget =
      kind === 'to_peer' ? { kind: 'to_peer' } : kind === 'from_peer' ? { kind: 'from_peer' } : { kind: 'off' };
    run(() => actions.setAudio(target));
  }

  function cycleAudio() {
    const available = audioChoices.filter((choice) => !choice.disabled).map((choice) => choice.value);
    const at = available.indexOf(audioKind);
    setAudio(available[(at + 1) % available.length]);
  }

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

  // ---- Devices ------------------------------------------------------------

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
      const message = await actions.installNotificationBridge(shellPath.trim());
      note(message);
      await refreshNotifications();
    });
  }

  // ---- Diagnostics -------------------------------------------------------

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
  <div class="hs">
    <div class="cols">
      <div class="col">
        <Card class="brand">
          <span class="label name">hyprKvm</span>
          <StatusDot state={online ? 'live' : ''} label={online ? 'Backend connected' : 'Backend not connected'} />
          <span class="label meta">
            {conn.status === 'connected' ? `v${conn.hello.backend_version} · ${conn.hello.platform}` : 'backend offline'}
          </span>
        </Card>

        <Card dots class="hero">
          <span class="label">{linked ? peer : snap?.local_name ?? draft?.device_name ?? 'This computer'}</span>
          <h1 class="display">{hero.word}</h1>
          <span class="label">
            {#if linked && snap?.round_trip_ms != null}{snap.round_trip_ms} ms · {/if}{settings?.local_address ?? ''}
          </span>
          <p>{heroLine}</p>
          {#if conn.status === 'absent'}
            <div class="actions">
              <Pill small onclick={() => run(actions.startBackend, 'Starting the backend…')}>Start backend</Pill>
            </div>
          {:else if online && snap?.state === 'paused'}
            <div class="actions"><Pill small onclick={() => run(actions.reconnect)}>Reconnect</Pill></div>
          {/if}
        </Card>

        <div class="tiles">
          <Tile caption="Remote" word="View" foot="coming later" disabled title="View isn't available yet" />
          <Tile
            caption="Control"
            word={controlTile.word}
            foot={controlTile.foot}
            dot={controlTile.dot}
            disabled={!snap?.can_switch_controller || snap.switching_controller}
            title="Hand control the other way"
            onclick={flipControl}
          />
          <Tile
            caption="Clipboard"
            word={clipboardWords[draft?.clipboard ?? 'off'][0]}
            foot={clipboardWords[draft?.clipboard ?? 'off'][1]}
            dot={draft && draft.clipboard !== 'off' ? 'on' : ''}
            disabled={!draft}
            title="Change what the clipboard shares (needs Save)"
            onclick={cycleClipboard}
          />
          <Tile
            caption="Audio"
            word={audioTile.word}
            foot={audioTile.foot}
            dot={audioTile.dot}
            disabled={!online || !!snap?.audio.from_phone}
            title="Change where audio goes"
            onclick={cycleAudio}
          />
        </div>

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

        {#if draft && settings && !settings.connector_only}
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
                  {#if fieldError('notifications')}<span class="err">{fieldError('notifications')}</span>{/if}
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
                    <span class="hint" style="flex:1" title={shellPath}>
                      {notifications.bridge_installed && notifications.active && !notifications.bridge_connected
                        ? 'Installed · not connected'
                        : notifications.bridge_installed
                          ? 'Bridge installed'
                          : 'Bridge installs on Save'}
                    </span>
                  {:else}
                    <input class="field" style="flex:1" placeholder="Path to shell.qml" aria-label="Quickshell shell.qml" bind:value={shellPath} />
                  {/if}
                  {#if notifications.bridge_installed}
                    <Pill small variant="ghost" onclick={reloadBridge}>Reload</Pill>
                  {/if}
                </span>
              </div>
            {/if}
          </Card>
        {/if}
      </div>

      <div class="col">
        {#if draft}
          <Card class="c">
            <Label>Sharing</Label>
            <div class="r">
              <span>Mouse and keyboard</span>
              <span class="ctl">
                <Switch
                  label="Share mouse and keyboard"
                  checked={snap?.input_forwarding ?? false}
                  disabled={!linked}
                  onchange={(enabled) => run(() => actions.setInputForwarding(enabled))}
                />
                <span class="hint">{linked ? `with ${peer}` : 'when connected'}</span>
              </span>
            </div>
            <div class="r">
              <span>Clipboard</span>
              <span class="ctl">
                <Segment
                  label="Clipboard"
                  value={draft.clipboard}
                  options={[
                    { value: 'off', label: 'Off' },
                    { value: 'text', label: 'Text' },
                    { value: 'text_and_images', label: '+ Images' }
                  ]}
                  onchange={(value) => draft && (draft.clipboard = value)}
                />
              </span>
            </div>
            {#if snap?.phones_supported ?? true}
              <div class="r">
                <span>Phone clipboard</span>
                <span class="ctl">
                  <Switch
                    label="Pass the phone's clipboard on"
                    checked={draft.forward_phone_clipboard}
                    disabled={draft.clipboard === 'off'}
                    onchange={(value) => draft && (draft.forward_phone_clipboard = value)}
                  />
                  <span class="hint">also send to {peerShort}</span>
                </span>
              </div>
            {/if}
            <div class="r">
              <span>Audio</span>
              <span class="ctl">
                <Segment
                  label="Audio"
                  value={audioKind}
                  options={audioChoices}
                  disabled={!online || !!snap?.audio.from_phone}
                  onchange={setAudio}
                />
                <span class="hint">Send plays this computer's sound on {peerShort}; Play brings {peerShort}'s here.</span>
                {#if snap && snap.phones.some((p) => p.connected && p.accepts_audio)}
                  <span class="ctl">
                    {#each snap.phones.filter((p) => p.connected && p.accepts_audio) as phone (phone.fingerprint)}
                      <Pill
                        small
                        variant={snap.audio.target.kind === 'to_phone' && snap.audio.target.fingerprint === phone.fingerprint ? '' : 'ghost'}
                        onclick={() => run(() => actions.setAudio({ kind: 'to_phone', fingerprint: phone.fingerprint }))}
                      >To {short(phone.name, 14)}</Pill>
                    {/each}
                  </span>
                {/if}
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
                <span class="hint">when sending to {peerShort}</span>
              </span>
            </div>
          </Card>
        {/if}

        <Card class="c">
          <Label>Devices</Label>
          <div class="r">
            <span>Computer</span>
            <span class="ctl">
              <span class="dev">
                <StatusDot state={linked ? 'live' : snap?.pairing_armed ? 'blink' : ''} />
                <span class="grow">
                  {snap?.peer_name || draft?.peer_host || 'None yet'}
                  <small>
                    {#if !online}backend offline
                    {:else if linked}connected{snap?.round_trip_ms != null ? ` · ${snap.round_trip_ms} ms` : ''}
                    {:else if snap?.pairing_armed}ready to pair: confirm on both computers
                    {:else if snap?.state === 'paused'}disconnected
                    {:else}waiting{/if}
                  </small>
                </span>
              </span>
              {#if linked}
                <Pill small variant="ghost" onclick={() => run(actions.disconnect)}>Disconnect</Pill>
              {:else if snap?.state === 'paused'}
                <Pill small variant="ghost" onclick={() => run(actions.reconnect)}>Reconnect</Pill>
              {:else}
                <Pill
                  small
                  variant="ghost"
                  disabled={!online || snap?.pairing_armed}
                  onclick={() => run(actions.armComputerPairing, 'Confirm on both computers')}
                >Pair a computer</Pill>
              {/if}
            </span>
          </div>
          {#if snap?.phones_supported ?? true}
            <div class="r">
              <span>Phones</span>
              <span class="ctl">
                <span class="devs">
                  {#each snap?.phones ?? [] as phone (phone.name)}
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
                        <Pill small variant="danger" onclick={() => (confirmRemove = phone.name)}>Remove…</Pill>
                      {/if}
                    </span>
                  {:else}
                    <span class="hint">{online ? 'None paired' : 'Start the backend to see phones'}</span>
                  {/each}
                  <span><Pill small disabled={!online} onclick={() => run(actions.openPhonePairing)}>+ Add phone</Pill></span>
                </span>
              </span>
            </div>
          {/if}
        </Card>

        <Card class="c">
          <Label>View <em>later</em></Label>
          <div class="r">
            <span>Let others view</span>
            <span class="ctl">
              <Switch label="Let others view this computer" checked={false} disabled />
              <span class="hint">Not available yet. Nothing on this screen is shared.</span>
            </span>
          </div>
        </Card>

        {#if draft && settings}
          <Card class="c">
            <Label>This computer</Label>
            <div class="r">
              <span>Name</span>
              <span class="ctl">
                <input class="field" class:bad={fieldError('device_name')} aria-label="Name" bind:value={draft.device_name} />
                {#if fieldError('device_name')}<span class="err">{fieldError('device_name')}</span>{/if}
              </span>
            </div>
            <div class="r">
              <span>Other computer</span>
              <span class="ctl">
                <input
                  class="field"
                  class:bad={fieldError('peer_host')}
                  aria-label="Other computer's address"
                  placeholder={settings.connector_only ? 'Address' : 'Address (optional: it can dial you)'}
                  bind:value={draft.peer_host}
                />
                {#if fieldError('peer_host')}<span class="err">{fieldError('peer_host')}</span>{/if}
              </span>
            </div>
            <div class="r">
              <span>Port</span>
              <span class="ctl">
                <input class="field" class:bad={fieldError('port')} style="max-width:110px" inputmode="numeric" aria-label="Port" bind:value={draft.port} />
                {#if fieldError('port')}<span class="err">{fieldError('port')}</span>{/if}
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

          <Card class="c" style="margin-bottom:{dirty ? 72 : 0}px">
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
                  {#each settings.audio_buffers_ms as ms (ms)}<option value={ms}>{ms} ms</option>{/each}
                </select>
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
                <span class="hint" title={settings.config_path}>{tail(settings.config_path, 44)}</span>
              </span>
            </div>
          </Card>
        {:else if settingsError}
          <Card class="c">
            <Label>Settings</Label>
            <div class="r"><span>Unavailable</span><span class="ctl"><span class="hint">{settingsError}</span></span></div>
          </Card>
        {/if}
      </div>
    </div>
  </div>

  <div class="savebar" hidden={!dirty}>
    <span class:bad={errors.length > 0}>
      {errors.length > 0 ? errors[0].message : online ? 'Restarts hyprKvm' : 'Saves for the next start'}
    </span>
    <Pill small variant="ghost" disabled={saving} onclick={discard}>Discard</Pill>
    <Pill small disabled={saving || errors.length > 0} onclick={save}>Save</Pill>
  </div>
  <div class="toast" class:show={toast.show} class:bad={toast.bad} role="status">{toast.text}</div>
</div>
