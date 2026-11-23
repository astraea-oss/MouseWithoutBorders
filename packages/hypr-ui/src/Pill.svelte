<script>
  /** @type {{ variant?: '' | 'ghost' | 'danger', small?: boolean, wide?: boolean, disabled?: boolean, onclick?: (e: MouseEvent) => void, children?: any, [key: string]: any }} */
  let { variant = '', small = false, wide = false, disabled = false, onclick, children, ...rest } = $props();
</script>

<button type="button" class="pill {variant}" class:sm={small} class:wide {disabled} {onclick} {...rest}>
  {@render children?.()}
</button>
