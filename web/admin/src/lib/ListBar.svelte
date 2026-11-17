<script lang="ts">
  // The search, the status filter and the page size of a list, and how many rows there are. The page owns the data and the request: this only holds what the person typed and says when
  // it changed (the text after a short pause, so that typing does not ask the server for every letter; a choice at once).
  import { onDestroy } from 'svelte';
  import { t } from '../i18n';

  let {
    q = $bindable(''),
    status = $bindable(''),
    limit = $bindable(100),
    statuses = [],
    total,
    shown,
    label,
    onchange,
  }: {
    q?: string;
    status?: string;
    limit?: number;
    /** The statuses the list can be narrowed to (value, shown text); none: no status filter. */
    statuses?: { value: string; label: string }[];
    total: number | null;
    shown: number;
    /** What the search box is called for a screen reader ("Search accounts"). */
    label: string;
    onchange: () => void;
  } = $props();

  const SIZES = [25, 50, 100];
  const PAUSE_MS = 300;
  let timer: ReturnType<typeof setTimeout> | null = null;

  function typed(): void {
    if (timer !== null) clearTimeout(timer);
    timer = setTimeout(() => {
      timer = null;
      onchange();
    }, PAUSE_MS);
  }

  function chosen(): void {
    if (timer !== null) clearTimeout(timer);
    timer = null;
    onchange();
  }

  function clear(): void {
    q = '';
    status = '';
    chosen();
  }

  onDestroy(() => {
    if (timer !== null) clearTimeout(timer);
  });

  const narrowed = $derived(q.trim() !== '' || status !== '');
</script>

<div class="list-bar" role="search">
  <label class="list-search">
    <span class="sr">{label}</span>
    <input type="search" bind:value={q} oninput={typed} placeholder={t('list.search')} autocapitalize="none" autocomplete="off" spellcheck="false" maxlength="64" />
  </label>
  {#if statuses.length > 0}
    <label>
      <span class="sr">{t('list.status')}</span>
      <select bind:value={status} onchange={chosen}>
        <option value="">{t('list.anyStatus')}</option>
        {#each statuses as s (s.value)}<option value={s.value}>{s.label}</option>{/each}
      </select>
    </label>
  {/if}
  <label>
    <span class="sr">{t('list.pageSize')}</span>
    <select bind:value={limit} onchange={chosen}>
      {#each SIZES as n (n)}<option value={n}>{t('list.perPage', { n })}</option>{/each}
    </select>
  </label>
  {#if narrowed}<button type="button" class="secondary" onclick={clear}>{t('list.clear')}</button>{/if}
  {#if total !== null}<span class="muted list-count" role="status">{t('list.showing', { shown, total })}</span>{/if}
</div>
