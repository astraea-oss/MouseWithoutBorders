<script lang="ts">
  import { goto } from '$app/navigation';
  import { Pill, StatusDot } from '@hyprkvm/hypr-ui';
  import { backend } from '../../lib/backend.svelte';

  const peer = $derived(backend.snapshot?.peer_name ?? 'the other computer');
</script>

<!-- View arrives in a later milestone; until then this says so plainly
     rather than drawing controls that do nothing. -->
<div class="app">
  <div class="top">
    <button class="ib" aria-label="Back" onclick={() => goto('/')}>
      <svg width="18" height="18" viewBox="0 0 24 24" style="transform:rotate(180deg)" aria-hidden="true">
        <path d="M4 12h15M13 6l6 6-6 6" stroke="currentColor" stroke-width="2.2" fill="none" stroke-linecap="square" />
      </svg>
    </button>
    <span class="ttl"><StatusDot /><span>View {peer}</span></span>
  </div>
  <div class="over">
    <div class="box">
      <StatusDot />
      <span class="display">Off</span>
      <p>View isn't available yet. It needs the other computer to share its focused window over this connection, which is still being built.</p>
      <Pill variant="ghost" onclick={() => goto('/')}>Back to Home</Pill>
    </div>
  </div>
</div>
