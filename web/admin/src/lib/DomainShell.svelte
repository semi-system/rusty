<script lang="ts">
  import type { Snippet } from 'svelte';
  import Link from './Link.svelte';
  import { href } from './router';
  import { nav } from './nav.svelte';
  import { t, type MessageKey } from '../i18n';

  let { name, children }: { name: string; children: Snippet } = $props();

  const tabs: { page: string; label: MessageKey }[] = [
    { page: 'overview', label: 'domain.overview' },
    { page: 'accounts', label: 'domain.accounts' },
    { page: 'aliases', label: 'domain.aliases' },
    { page: 'routing', label: 'domain.routing' },
    { page: 'dkim', label: 'domain.dkim' },
    { page: 'dns', label: 'domain.dns' },
    { page: 'relay', label: 'domain.relay' },
    { page: 'storage', label: 'domain.storage' },
  ];
</script>

<header class="page-head">
  <h1>{name}</h1>
</header>
<nav class="tabs" aria-label={t('domain.tabs', { name })}>
  {#each tabs as tab (tab.page)}
    {@const to = href('/domains/{name}/' + tab.page, { name })}
    <Link href={to} current={nav.path === to}>{t(tab.label)}</Link>
  {/each}
</nav>
{@render children()}
