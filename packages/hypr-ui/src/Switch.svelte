<script>
  /**
   * An on/off switch. `red` is for switches that make something live.
   * @type {{ checked?: boolean, red?: boolean, disabled?: boolean, label: string, onchange?: (checked: boolean) => void }}
   */
  let { checked = false, red = false, disabled = false, label, onchange } = $props();
</script>

<button
  type="button"
  role="switch"
  class="switch"
  class:on={checked}
  class:red
  aria-checked={checked}
  aria-label={label}
  {disabled}
  onclick={() => onchange?.(!checked)}
></button>
