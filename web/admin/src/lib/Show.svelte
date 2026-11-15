<script lang="ts" generics="T">
  import type { Snippet } from 'svelte';
  import type { Load } from './load.svelte';
  import ErrorNote from './ErrorNote.svelte';
  import { t } from '../i18n';

  let { load, children }: { load: Load<T>; children: Snippet<[T]> } = $props();
</script>

{#if load.state.status === 'loading'}
  <p class="muted" role="status">{t('app.loading')}</p>
{:else if load.state.status === 'error'}
  <ErrorNote error={load.state.error} />
{:else}
  {@render children(load.state.data)}
{/if}
