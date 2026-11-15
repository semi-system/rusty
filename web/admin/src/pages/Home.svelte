<script lang="ts">
  import type { App } from '../lib/app.svelte';
  import type { Me } from '../api/types';
  import { t } from '../i18n';

  let { app, me }: { app: App; me: Me } = $props();
</script>

<header class="bar">
  <nav aria-label={t('nav.main')}>
    <strong>{t('app.title')}</strong>
    <span>{t('nav.domains')}</span>
  </nav>
  <button type="button" onclick={() => app.signOut()}>{t('nav.signOut')}</button>
</header>
<main id="content" tabindex="-1">
  <h1>{t('home.title')}</h1>
  <p>{t('home.signedInAs', { address: me.address })}</p>
  <p class="muted">{me.scope === 'platform' ? t('home.scopePlatform') : t('home.scopeDomain')}</p>
</main>
