<script lang="ts">
  import '@hyprkvm/hypr-ui/fonts.css';
  import '@hyprkvm/hypr-ui/theme.css';
  import '../app.css';
  import { onMount } from 'svelte';
  import { goto } from '$app/navigation';
  import { page } from '$app/state';
  import { listen } from '@tauri-apps/api/event';
  import { connectBackend } from '../lib/backend.svelte';

  let { children } = $props();

  onMount(() => {
    connectBackend();
    // Tray and second launches choose a screen.
    const unlisten = listen<string>('ui://navigate', (event) => goto(event.payload));
    return () => {
      unlisten.then((stop) => stop());
    };
  });

  /** Back, Esc, mouse back and Alt+Left all go home from a sub-screen. */
  function back(event: Event) {
    if (page.url.pathname === '/' || page.url.pathname === '/pair-phone') return;
    event.preventDefault();
    goto('/');
  }

  function keydown(event: KeyboardEvent) {
    const typing = event.target instanceof HTMLInputElement || event.target instanceof HTMLSelectElement;
    if ((event.key === 'Escape' && !typing) || (event.altKey && event.key === 'ArrowLeft')) back(event);
  }

  function mouseup(event: MouseEvent) {
    if (event.button === 3) back(event);
  }
</script>

<svelte:window onkeydown={keydown} onmouseup={mouseup} />

{@render children()}
