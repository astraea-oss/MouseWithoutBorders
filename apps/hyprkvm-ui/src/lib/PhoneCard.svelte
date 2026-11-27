<script lang="ts">
  // A two-wide Home card for a phone, in the tiles' language: caption, one
  // dot-matrix word and a footer, beside a phone drawn in dots, with
  // small buttons for what can be done to the phone right now. With no phone
  // paired it offers to add one.
  import type { PhoneState } from './types';

  let {
    phone = null,
    audio = '',
    disabled = false,
    onclick,
    onsend,
    onstopview
  }: {
    /** The paired phone, or null for the "add a phone" card. */
    phone?: PhoneState | null;
    /** Audio to or from this phone right now. */
    audio?: '' | 'to' | 'from';
    disabled?: boolean;
    /** The card itself: Settings for a paired phone, pairing otherwise. */
    onclick?: () => void;
    /** Send this computer's clipboard to the phone; no button without it. */
    onsend?: () => void;
    /** End the phone's Remote View of this computer. */
    onstopview?: () => void;
  } = $props();

  // The phone in dots: an outline with the corners rounded off, a speaker
  // slot at the top and a home bar at the bottom, the screen left empty so
  // it reads as a phone at a glance.
  const COLS = 7;
  const ROWS = 13;
  const PITCH = 5;

  type Dot = { x: number; y: number; kind: 'edge' | 'detail' | 'screen'; c: number; r: number };

  const grid: Dot[] = [];
  for (let r = 0; r < ROWS; r++) {
    for (let c = 0; c < COLS; c++) {
      const edgeRow = r === 0 || r === ROWS - 1;
      const edgeCol = c === 0 || c === COLS - 1;
      let kind: Dot['kind'] | null = null;
      if (edgeRow && edgeCol) kind = null;
      else if (edgeRow || edgeCol) kind = 'edge';
      else if ((r === 1 || r === ROWS - 2) && c >= 2 && c <= 4) kind = 'detail';
      else if (r >= 3 && r <= ROWS - 4) kind = 'screen';
      if (kind) grid.push({ x: 2.5 + c * PITCH, y: 2.5 + r * PITCH, kind, c, r });
    }
  }

  const state = $derived(!phone ? 'add' : phone.connected ? 'on' : 'off');

  /** The add card dashes its outline and draws a plus on the screen. */
  function shown(dot: Dot) {
    if (dot.kind === 'screen') {
      return state === 'add' && ((dot.c === 3 && dot.r >= 4 && dot.r <= 8) || (dot.r === 6 && dot.c >= 2 && dot.c <= 4));
    }
    if (dot.kind === 'edge' && state === 'add') return (dot.c + dot.r) % 2 === 0;
    return true;
  }

  const dots = $derived(grid.filter(shown));
  const word = $derived({ add: 'Connect', on: 'Linked', off: 'Offline' }[state]);
  const viewing = $derived(!!phone?.connected && phone.viewing);
</script>

<div class="card phonecard {state}">
  <button type="button" class="main" {disabled} {onclick} aria-label={phone ? `${phone.name} settings` : 'Add a phone'}>
    <svg class="device" viewBox="0 0 35 65" aria-hidden="true">
      {#each dots as d (d.c * 100 + d.r)}
        <circle cx={d.x} cy={d.y} r="1.6" class={d.kind} />
      {/each}
    </svg>
    <span class="body">
      <span class="label">{phone ? phone.name : 'Phone'}</span>
      <span class="display word">{word}</span>
      <span class="label foot">
        {#if !phone}
          pair a phone
        {:else if phone.connected}
          <span>{phone.round_trip_ms != null ? `${phone.round_trip_ms} ms` : 'linked'}</span>
          {#if viewing}<span class="live">{phone.controlling ? 'controls' : 'viewing'}</span>{/if}
          {#if audio}<span class="live">{audio === 'from' ? 'audio in' : 'audio'}</span>{/if}
        {:else}
          not connected
        {/if}
      </span>
    </span>
  </button>
  {#if phone?.connected && (viewing || onsend)}
    <span class="acts">
      {#if viewing}
        <button
          type="button"
          class="ib sm live"
          aria-label="Stop Remote View"
          onclick={onstopview}
        >
          <svg width="16" height="16" viewBox="0 0 24 24" aria-hidden="true">
            <path d="M2 12s3.6-6.5 10-6.5S22 12 22 12s-3.6 6.5-10 6.5S2 12 2 12z" stroke="currentColor" stroke-width="2" fill="none" />
            <circle cx="12" cy="12" r="2.6" fill="currentColor" />
          </svg>
        </button>
      {/if}
      {#if onsend}
        <button type="button" class="ib sm" aria-label="Send clipboard to phone" onclick={onsend}>
          <svg width="16" height="16" viewBox="0 0 24 24" aria-hidden="true">
            <path d="M9 4h6v3H9zM7 5.5H5v15h14v-15h-2M12 17.5v-7M9 13.5l3-3 3 3" stroke="currentColor" stroke-width="2" fill="none" stroke-linecap="square" />
          </svg>
        </button>
      {/if}
    </span>
  {/if}
</div>

<style>
  .phonecard { grid-column: 1 / -1; display: flex; align-items: flex-end; position: relative; min-width: 0; padding: 0; }
  .phonecard:has(.main:hover:not(:disabled)) { background: #161616; }
  .main {
    flex: 1; display: flex; align-items: center; gap: var(--tile-pad, 12px); min-width: 0; padding: var(--tile-pad, 12px);
    border: none; background: none; text-align: left; color: inherit; border-radius: inherit;
  }
  .main:disabled { opacity: .45; }
  .acts { display: flex; gap: 6px; flex: none; padding: 0 var(--tile-pad, 12px) var(--tile-pad, 12px) 0; }
  .ib.live { color: var(--red); border-color: var(--red); }

  .device { width: 35px; height: 65px; flex: none; }
  .device circle.edge, .device circle.screen { fill: var(--white); }
  .device circle.detail { fill: var(--grey); }
  .off .device circle.edge { fill: var(--grey-dim); }
  .off .device circle.detail { fill: #333; }
  .add .device circle.edge, .add .device circle.detail { fill: var(--grey); }

  .body { display: flex; flex-direction: column; gap: 8px; min-width: 0; flex: 1; }
  .body .label { font-size: var(--tile-label, 9px); letter-spacing: .1em; max-width: 100%; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .word { font-size: var(--tile-word, 30px); max-width: 100%; overflow: hidden; white-space: nowrap; }
  .off .word { color: var(--grey); }
  .foot { display: flex; gap: 10px; }
  .foot span { color: var(--grey-dim); }
  .foot span:first-child { color: var(--grey); }
  .foot span.live { color: var(--red); }

</style>
