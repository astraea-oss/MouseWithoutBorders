<script lang="ts">
  import { goto } from '$app/navigation';
  import { Card, Tile } from '@hyprkvm/hypr-ui';
  import { actions, backend } from '../lib/backend.svelte';
  import { note, run, short } from '../lib/feedback.svelte';
  import Overlays from '../lib/Overlays.svelte';
  import PhoneCard from '../lib/PhoneCard.svelte';
  import { settings } from '../lib/settings.svelte';
  import type { AudioTarget, ClipboardMode, PhoneState } from '../lib/types';

  settings.start();

  const snap = $derived(backend.snapshot);
  const conn = $derived(backend.connection);
  const draft = $derived(settings.draft);
  const online = $derived(conn.status === 'connected' && snap !== null);
  const linked = $derived(online && snap?.state === 'connected');
  const peer = $derived(snap?.peer_name || draft?.peer_host || 'the other computer');
  const peerShort = $derived(short(snap?.peer_name || draft?.peer_host || 'other PC', 10));

  // ---- Hero: who, the state in one word, and where ------------------------

  const hero = $derived.by(() => {
    switch (conn.status) {
      case 'connected':
        if (!snap) return { word: '…' };
        return {
          connected: { word: 'Linked' },
          listening: snap.pairing_armed
            ? { word: 'Pairing' }
            : { word: 'Waiting' },
          paused: { word: 'Paused' },
          error: { word: 'Error' },
          starting: { word: 'Starting' }
        }[snap.state];
      case 'absent':
        return { word: 'Offline' };
      case 'starting':
      case 'connecting':
        return { word: 'Starting' };
      case 'disconnected':
        return { word: 'Lost' };
      case 'incompatible':
        return { word: 'Update' };
      case 'other_instance':
        return { word: 'Elsewhere' };
    }
  });

  const heroLine = $derived.by(() => {
    switch (conn.status) {
      case 'connected':
        return snap?.summary ?? '';
      case 'absent':
        return "hyprKvm's backend isn't running. Settings still save for its next start.";
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

  /** Under the peer's name, its address and latency; otherwise this
   *  computer's address, which is what the other side needs to type. */
  const heroMeta = $derived(
    linked
      ? [settings.base?.peer_host ?? '', snap?.round_trip_ms != null ? `${snap.round_trip_ms} ms` : '']
          .filter(Boolean)
          .join(' · ')
      : (settings.view?.local_address ?? '')
  );

  // ---- Tiles ----------------------------------------------------------------

  // Tiles carry a dot only when it says something the word can't: red while
  // the mouse and keyboard are actually across on the other computer, and a
  // blink while control changes hands.
  const controlTile = $derived.by(() => {
    if (!linked || !snap) return { word: '—', foot: 'not connected', dot: '' as const };
    const here = snap.controller === 'local';
    if (snap.switching_controller) return { word: '…', foot: 'switching', dot: 'blink' as const };
    if (!snap.input_forwarding) return { word: 'Off', foot: 'mouse stays here', dot: '' as const };
    const dot = snap.remote_input_active ? ('live' as const) : ('' as const);
    return here
      ? { word: 'Here', foot: `you drive ${peerShort}`, dot }
      : { word: peerShort, foot: `${peerShort} drives you`, dot };
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
    draft.clipboard = clipboardOrder[(clipboardOrder.indexOf(draft.clipboard) + 1) % clipboardOrder.length];
  }

  const audioTile = $derived.by(() => {
    if (!snap) return { word: 'Off', foot: 'not connected' };
    if (snap.audio.from_phone) {
      const phone = snap.phones.find((p) => p.fingerprint === snap.audio.from_phone);
      return { word: 'Phone', foot: `from ${short(phone?.name ?? 'a phone', 12)}` };
    }
    const target = snap.audio.target;
    switch (target.kind) {
      case 'to_peer':
        return { word: 'Send', foot: `to ${peerShort}` };
      case 'from_peer':
        return { word: 'Play', foot: `from ${peerShort}` };
      case 'to_phone': {
        const phone = snap.phones.find((p) => p.fingerprint === target.fingerprint);
        return { word: 'Send', foot: `to ${short(phone?.name ?? 'phone', 12)}` };
      }
      default:
        return { word: 'Off', foot: 'no audio' };
    }
  });

  /** Off → Send → Play → each phone that takes audio → Off. */
  function cycleAudio() {
    if (!snap) return;
    const targets: AudioTarget[] = [
      { kind: 'off' },
      ...(snap.audio.to_peer_available ? [{ kind: 'to_peer' } as const] : []),
      ...(snap.audio.from_peer_available ? [{ kind: 'from_peer' } as const] : []),
      ...snap.phones
        .filter((phone) => phone.connected && phone.accepts_audio)
        .map((phone) => ({ kind: 'to_phone', fingerprint: phone.fingerprint }) as const)
    ];
    const current = snap.audio.target;
    const at = targets.findIndex(
      (target) =>
        target.kind === current.kind &&
        (target.kind !== 'to_phone' || (current.kind === 'to_phone' && target.fingerprint === current.fingerprint))
    );
    const next = targets[(at + 1) % targets.length];
    run(() => actions.setAudio(next));
  }

  // ---- Phones -----------------------------------------------------------------

  function phoneAudio(phone: PhoneState): '' | 'to' | 'from' {
    if (!snap) return '';
    if (snap.audio.target.kind === 'to_phone' && snap.audio.target.fingerprint === phone.fingerprint) return 'to';
    // from_phone is the streaming phone's fingerprint.
    if (snap.audio.from_phone === phone.fingerprint) return 'from';
    return '';
  }
</script>

<div class="app">
  <div class="hs" class:dirty={settings.dirty}>
    <div class="cols">
      <div class="col">
        <Card class="brand">
          <span class="label name">hyprKvm</span>
          <span class="label meta">
            {conn.status === 'connected' ? `v${conn.hello.backend_version} · ${conn.hello.platform}` : 'backend offline'}
          </span>
          <button class="ib sm" aria-label="Settings" onclick={() => goto('/settings')}>
            <svg width="16" height="16" viewBox="0 0 24 24" aria-hidden="true"><path d="M4 7h9M17 7h3M4 17h3M11 17h9" stroke="currentColor" stroke-width="2" fill="none" stroke-linecap="square" /><circle cx="15" cy="7" r="2.2" stroke="currentColor" stroke-width="2" fill="none" /><circle cx="9" cy="17" r="2.2" stroke="currentColor" stroke-width="2" fill="none" /></svg>
          </button>
        </Card>

        <Card dots class="hero">
          <div class="hero-top">
            <span class="label">{linked ? peer : (snap?.local_name ?? draft?.device_name ?? 'This computer')}</span>
            <span class="acts">
              {#if conn.status === 'absent'}
                <button class="ib sm" aria-label="Start backend" onclick={() => run(actions.startBackend, 'Starting the backend…')}>
                  <svg width="16" height="16" viewBox="0 0 24 24" aria-hidden="true"><path d="M8 5l11 7-11 7z" fill="currentColor" /></svg>
                </button>
              {:else if linked}
                <button
                  class="ib sm"
                  class:on={!snap?.input_forwarding}
                  aria-label={snap?.input_forwarding ? 'Pause mouse and keyboard' : 'Resume mouse and keyboard'}
                  onclick={() => run(() => actions.setInputForwarding(!snap?.input_forwarding))}
                >
                  {#if snap?.input_forwarding}
                    <svg width="16" height="16" viewBox="0 0 24 24" aria-hidden="true"><path d="M9 6v12M15 6v12" stroke="currentColor" stroke-width="2.4" fill="none" stroke-linecap="square" /></svg>
                  {:else}
                    <svg width="16" height="16" viewBox="0 0 24 24" aria-hidden="true"><path d="M8 5l11 7-11 7z" fill="currentColor" /></svg>
                  {/if}
                </button>
                <button class="ib sm" aria-label="Disconnect" onclick={() => run(actions.disconnect)}>
                  <svg width="16" height="16" viewBox="0 0 24 24" aria-hidden="true"><path d="M12 3v8M6.6 7.2a7.5 7.5 0 1 0 10.8 0" stroke="currentColor" stroke-width="2.2" fill="none" stroke-linecap="square" /></svg>
                </button>
              {:else if online && snap?.state === 'paused'}
                <button class="ib sm" aria-label="Reconnect" onclick={() => run(actions.reconnect)}>
                  <svg width="16" height="16" viewBox="0 0 24 24" aria-hidden="true"><path d="M19.5 12a7.5 7.5 0 1 1-2.2-5.3M19.5 4v4.5H15" stroke="currentColor" stroke-width="2.2" fill="none" stroke-linecap="square" /></svg>
                </button>
              {:else if online}
                <button
                  class="ib sm"
                  aria-label="Pair a computer"
                  disabled={snap?.pairing_armed}
                  onclick={() => run(actions.armComputerPairing, 'Confirm on both computers')}
                >
                  <svg width="16" height="16" viewBox="0 0 24 24" aria-hidden="true"><path d="M12 5v14M5 12h14" stroke="currentColor" stroke-width="2.2" fill="none" stroke-linecap="square" /></svg>
                </button>
              {/if}
            </span>
          </div>
          <h1 class="display">{hero.word}</h1>
          {#if heroMeta}<span class="label">{heroMeta}</span>{/if}
          <!-- Linked, the tiles say the rest; otherwise say what's going on. -->
          {#if !linked && heroLine}<p>{heroLine}</p>{/if}
        </Card>
      </div>

      <div class="col">
        <div class="tiles">
          <Tile caption="Remote" word="View" foot="coming later" disabled />
          <Tile
            caption="Control"
            word={controlTile.word}
            foot={controlTile.foot}
            dot={controlTile.dot}
            disabled={!snap?.can_switch_controller || snap.switching_controller}
            onclick={flipControl}
          />
          <Tile
            caption="Clipboard"
            word={clipboardWords[draft?.clipboard ?? 'off'][0]}
            foot={clipboardWords[draft?.clipboard ?? 'off'][1]}
            disabled={!draft}
            onclick={cycleClipboard}
          />
          <Tile
            caption="Audio"
            word={audioTile.word}
            foot={audioTile.foot}
            disabled={!online || !!snap?.audio.from_phone}
            onclick={cycleAudio}
          />
          {#if snap?.phones_supported ?? true}
            {#each snap?.phones ?? [] as phone (phone.fingerprint)}
              <PhoneCard
                {phone}
                audio={phoneAudio(phone)}
                onclick={() => goto('/settings')}
                onsend={draft?.clipboard !== 'off'
                  ? () => run(async () => note(await actions.sendClipboardToPhone(phone.fingerprint)))
                  : undefined}
                onstopview={() => run(() => actions.stopPhoneView(phone.fingerprint), 'Stopped Remote View')}
              />
            {:else}
              <PhoneCard disabled={!online} onclick={() => run(actions.openPhonePairing)} />
            {/each}
          {/if}
        </div>
      </div>
    </div>
  </div>
  <Overlays />
</div>
