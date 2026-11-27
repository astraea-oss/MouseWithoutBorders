<script>
  /**
   * A segmented choice. Options are `{ value, label, disabled? }`.
   * @type {{ options: { value: any, label: string, disabled?: boolean }[], value: any, label: string, fit?: boolean, disabled?: boolean, onchange?: (value: any) => void }}
   */
  let { options, value, label, fit = false, disabled = false, onchange } = $props();
</script>

<div class="seg" class:fit role="radiogroup" aria-label={label}>
  {#each options as option (option.value)}
    <button
      type="button"
      role="radio"
      class:on={option.value === value}
      aria-checked={option.value === value}
      disabled={disabled || option.disabled}
      onclick={() => option.value !== value && onchange?.(option.value)}
    >{option.label}</button>
  {/each}
</div>
