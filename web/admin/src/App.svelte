<script lang="ts">
  import { onMount } from 'svelte';
  import type { App } from './lib/app.svelte';
  import Login from './pages/Login.svelte';
  import Setup from './pages/Setup.svelte';
  import Home from './pages/Home.svelte';
  import ErrorNote from './lib/ErrorNote.svelte';
  import { t } from './i18n';

  let { app }: { app: App } = $props();

  onMount(() => {
    void app.boot();
  });
</script>

<a class="skip" href="#content">{t('app.skip')}</a>
{#if app.view.kind === 'loading'}
  <main id="content"><p class="muted" role="status">{t('app.loading')}</p></main>
{:else if app.view.kind === 'failed'}
  <main id="content"><ErrorNote error={app.view.error} /></main>
{:else if app.view.kind === 'setup'}
  <Setup {app} />
{:else if app.view.kind === 'signin'}
  <Login {app} />
{:else}
  <Home {app} me={app.view.me} />
{/if}
