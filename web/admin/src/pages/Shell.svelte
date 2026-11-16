<script lang="ts">
  import type { App } from '../lib/app.svelte';
  import type { Me } from '../api/types';
  import { nav } from '../lib/nav.svelte';
  import { matchRoute, type Route } from '../lib/router';
  import { t } from '../i18n';
  import Domains from './Domains.svelte';
  import Overview from './Overview.svelte';
  import Accounts from './Accounts.svelte';
  import Aliases from './Aliases.svelte';
  import Dkim from './Dkim.svelte';
  import Dns from './Dns.svelte';
  import Relay from './Relay.svelte';
  import Storage from './Storage.svelte';
  import MonitorPage from './Monitor.svelte';
  import NotFound from './NotFound.svelte';
  import Link from '../lib/Link.svelte';

  let { app, me }: { app: App; me: Me } = $props();

  // A route never ends in a domain name: the server answers a path that looks like a file with 404 (see docs/admin-web-frontend.md).
  type Page = 'domains' | 'overview' | 'accounts' | 'aliases' | 'dkim' | 'dns' | 'relay' | 'storage' | 'monitor';
  const routes: Route<Page>[] = [
    { pattern: '/', value: 'domains' },
    { pattern: '/monitor', value: 'monitor' },
    { pattern: '/domains/{name}/overview', value: 'overview' },
    { pattern: '/domains/{name}/accounts', value: 'accounts' },
    { pattern: '/domains/{name}/aliases', value: 'aliases' },
    { pattern: '/domains/{name}/dkim', value: 'dkim' },
    { pattern: '/domains/{name}/dns', value: 'dns' },
    { pattern: '/domains/{name}/relay', value: 'relay' },
    { pattern: '/domains/{name}/storage', value: 'storage' },
  ];
  const match = $derived(matchRoute(routes, nav.path));
  const name = $derived(match?.params.name ?? '');
</script>

<header class="bar">
  <nav aria-label={t('nav.main')}>
    <strong>{t('app.title')}</strong>
    <Link href="/" current={nav.path === '/'}>{t('nav.domains')}</Link>
    {#if me.scope === 'platform'}<Link href="/monitor" current={nav.path === '/monitor'}>{t('nav.monitor')}</Link>{/if}
  </nav>
  <span>
    <span class="muted">{t('home.signedInAs', { address: me.address })} · {me.scope === 'platform' ? t('home.scopePlatform') : t('home.scopeDomain')}</span>
    <button type="button" onclick={() => app.signOut()}>{t('nav.signOut')}</button>
  </span>
</header>

{#if match === null}
  <NotFound />
{:else if match.value === 'domains'}
  <Domains {app} {me} />
{:else if match.value === 'monitor'}
  <!-- Only for platform administrators: for anyone else the page does not exist (the API says 403 as well). -->
  {#if me.scope === 'platform'}<MonitorPage {app} />{:else}<NotFound />{/if}
{:else}
  <!-- Keyed by the domain and the page: moving to another domain starts the page afresh. -->
  {#key nav.path}
    {#if match.value === 'overview'}
      <Overview {app} {me} {name} />
    {:else if match.value === 'accounts'}
      <Accounts {app} {name} />
    {:else if match.value === 'aliases'}
      <Aliases {app} {name} />
    {:else if match.value === 'dkim'}
      <Dkim {app} {name} />
    {:else if match.value === 'dns'}
      <Dns {app} {name} />
    {:else if match.value === 'relay'}
      <Relay {app} {name} />
    {:else}
      <Storage {app} {me} {name} />
    {/if}
  {/key}
{/if}
