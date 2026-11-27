<script lang="ts">
  // The Save bar and the toast, floating over Home and Settings.
  import { Pill } from '@hyprkvm/hypr-ui';
  import { backend } from './backend.svelte';
  import { short, toast } from './feedback.svelte';
  import { settings } from './settings.svelte';

  const online = $derived(backend.connection.status === 'connected' && backend.snapshot !== null);
  const linked = $derived(online && backend.snapshot?.state === 'connected');
  const peer = $derived(short(backend.snapshot?.peer_name || settings.draft?.peer_host || 'other PC', 10));
  // Saving restarts the backend, which drops the link for a few seconds.
  const saveNote = $derived(
    !online ? 'Saves for the next start' : linked ? `Saving reconnects to ${peer} (a few seconds)` : 'Saving restarts the backend'
  );
</script>

<div class="savebar" hidden={!settings.dirty}>
  <span class:bad={settings.errors.length > 0}>
    {settings.errors.length > 0 ? settings.errors[0].message : saveNote}
  </span>
  <Pill small variant="ghost" disabled={settings.saving} onclick={() => settings.discard()}>Discard</Pill>
  <Pill small disabled={settings.saving || settings.errors.length > 0} onclick={() => settings.save()}>Save</Pill>
</div>
<div class="toast" class:show={toast.show} class:bad={toast.bad} role="status">{toast.text}</div>
