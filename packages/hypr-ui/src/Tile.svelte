<script>
  import StatusDot from './StatusDot.svelte';

  /**
   * A square Home tile: caption, corner dot, one big word, and a footer.
   * @type {{ caption: string, word: string, foot?: string, dot?: '' | 'on' | 'live' | 'blink', fresh?: boolean, disabled?: boolean, title?: string, onclick?: () => void }}
   */
  let { caption, word, foot = '', dot = '', fresh = false, disabled = false, title, onclick } = $props();
</script>

<button type="button" class="card tile" class:fresh {disabled} {title} {onclick}>
  <span class="label">{caption}</span>
  <span class="corner"><StatusDot state={dot} /></span>
  <span class="display word">{word}</span>
  <span class="label">{foot}</span>
</button>

<style>
  .tile {
    aspect-ratio: 1; display: flex; flex-direction: column; justify-content: space-between;
    align-items: flex-start; border: none; text-align: left; color: inherit; position: relative;
    min-width: 0;
  }
  .tile:hover:not(:disabled) { background: #161616; }
  .tile:disabled { opacity: .45; }
  .tile.fresh { outline: 1px solid var(--grey-dim); outline-offset: 3px; }
  .word { font-size: var(--tile-word, 28px); max-width: 100%; overflow: hidden; text-overflow: clip; white-space: nowrap; }
  .label { font-size: var(--tile-label, 9px); letter-spacing: .1em; max-width: 100%; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .corner { position: absolute; right: var(--tile-pad, 12px); top: var(--tile-pad, 12px); }
  .tile { padding: var(--tile-pad, 12px); }
</style>
