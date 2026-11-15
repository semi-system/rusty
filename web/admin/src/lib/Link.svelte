<script lang="ts">
  import type { Snippet } from 'svelte';
  import { nav } from './nav.svelte';

  let { href, current = false, children }: { href: string; current?: boolean; children: Snippet } = $props();

  function click(e: MouseEvent): void {
    // A plain left click is handled here; a modified click (new tab, window) is left to the browser.
    if (e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
    e.preventDefault();
    nav.go(href);
  }
</script>

<a {href} onclick={click} aria-current={current ? 'page' : undefined}>{@render children()}</a>
