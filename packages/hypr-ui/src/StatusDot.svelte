<script>
  /**
   * '' idle (grey), 'on' (white), 'live' (red: a stream, captured keys),
   * 'blink' (red, waiting). Red means live and nothing else.
   * @type {{ state?: '' | 'on' | 'live' | 'blink', label?: string }}
   */
  let { state = '', label } = $props();
</script>

<span class="dot {state}" role={label ? 'img' : undefined} aria-label={label} aria-hidden={label ? undefined : 'true'}></span>
