<script>
  /** A surface card. `dots` adds the dot-matrix backdrop used by hero cards. */
  let { dots = false, class: extra = '', children, ...rest } = $props();
</script>

<div class="card {extra}" class:dots {...rest}>{@render children?.()}</div>
