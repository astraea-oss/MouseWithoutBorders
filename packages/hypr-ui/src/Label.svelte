<script>
  /** Small uppercase mono text: card headings, captions, metadata. */
  let { class: extra = '', children } = $props();
</script>

<span class="label {extra}">{@render children?.()}</span>
