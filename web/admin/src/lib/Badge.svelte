<script lang="ts">
  import type { Snippet } from 'svelte';

  // A status as a word with a coloured dot: the colour is never the only signal (the word says it too).
  let { tone = 'neutral', children }: { tone?: 'neutral' | 'ok' | 'warn' | 'danger'; children: Snippet } = $props();
</script>

<span class={tone === 'neutral' ? 'badge' : 'badge ' + tone}>{@render children()}</span>
