<script lang="ts">
  import type { Snippet } from 'svelte';

  // The head of a page: its title (the one h1), a line that says what the page is for, a status next to the title, and the main actions on the right.
  let { title, lede = undefined, badge = undefined, actions = undefined }: { title: string; lede?: string; badge?: Snippet; actions?: Snippet } = $props();
</script>

<header class="page-head">
  <div>
    <div class="page-title-row">
      <h1>{title}</h1>
      {#if badge}{@render badge()}{/if}
    </div>
    {#if lede}<p class="lede">{lede}</p>{/if}
  </div>
  {#if actions}<div class="page-actions">{@render actions()}</div>{/if}
</header>
