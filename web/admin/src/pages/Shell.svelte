<script lang="ts">
  import { onMount } from 'svelte';
  import type { App } from '../lib/app.svelte';
  import type { Me } from '../api/types';
  import { nav } from '../lib/nav.svelte';
  import { href, matchRoute, type Route } from '../lib/router';
  import { t, type MessageKey } from '../i18n';
  import Domains from './Domains.svelte';
  import Overview from './Overview.svelte';
  import Accounts from './Accounts.svelte';
  import Aliases from './Aliases.svelte';
  import Dkim from './Dkim.svelte';
  import Dns from './Dns.svelte';
  import Relay from './Relay.svelte';
  import Storage from './Storage.svelte';
  import MonitorPage from './Monitor.svelte';
  import ProtocolsPage from './Protocols.svelte';
  import PlansPage from './Plans.svelte';
  import Administrators from './Administrators.svelte';
  import NotFound from './NotFound.svelte';
  import Link from '../lib/Link.svelte';

  let { app, me }: { app: App; me: Me } = $props();

  // A route never ends in a domain name: the server answers a path that looks like a file with 404 (see docs/admin-web-frontend.md).
  type Page = 'domains' | 'overview' | 'accounts' | 'aliases' | 'dkim' | 'dns' | 'relay' | 'storage' | 'monitor' | 'protocols' | 'plans' | 'administrators';
  const routes: Route<Page>[] = [
    { pattern: '/', value: 'domains' },
    { pattern: '/monitor', value: 'monitor' },
    { pattern: '/protocols', value: 'protocols' },
    { pattern: '/plans', value: 'plans' },
    { pattern: '/administrators', value: 'administrators' },
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

  // Where the person is, from the top: the list, the domain, the page of the domain.
  const pageLabel: Record<string, MessageKey> = {
    overview: 'domain.overview',
    accounts: 'domain.accounts',
    aliases: 'domain.aliases',
    dkim: 'domain.dkim',
    dns: 'domain.dns',
    relay: 'domain.relay',
    storage: 'domain.storage',
  };
  type Crumb = { label: string; to?: string };
  const crumbs = $derived.by((): Crumb[] => {
    if (match === null) return [];
    if (match.value === 'domains') return [{ label: t('nav.domains') }];
    if (match.value === 'monitor') return [{ label: t('nav.monitor') }];
    if (match.value === 'protocols') return [{ label: t('nav.protocols') }];
    if (match.value === 'plans') return [{ label: t('nav.plans') }];
    if (match.value === 'administrators') return [{ label: t('nav.administrators') }];
    const key = pageLabel[match.value];
    return [{ label: t('nav.domains'), to: '/' }, { label: name, to: href('/domains/{name}/overview', { name }) }, { label: key ? t(key) : '' }];
  });

  // One stream of hints for the whole console while someone is signed in (the frame lives exactly that long).
  onMount(() => {
    app.live.start();
    return () => app.live.stop();
  });

  // On a narrow screen the sidebar is a drawer: opened by the menu button, closed by a choice, the scrim or Escape.
  let menuOpen = $state(false);
  $effect(() => {
    void nav.path;
    menuOpen = false;
  });
</script>

<svelte:window onkeydown={(e) => { if (e.key === 'Escape') menuOpen = false; }} />

<div class="app">
  <aside class="side" class:open={menuOpen} id="sidebar">
    <div class="brand">
      <span class="brand-mark" aria-hidden="true">{t('app.mark')}</span>
      <div>
        <div class="brand-name">{t('app.title')}</div>
        <div class="brand-sub">{t('app.productSub')}</div>
      </div>
    </div>
    <nav aria-label={t('nav.main')}>
      <div class="nav-group">
        <h2>{t('nav.manage')}</h2>
        <Link class="nav-link" href="/" current={nav.path === '/' || nav.path.startsWith('/domains/')}>{t('nav.domains')}</Link>
        <Link class="nav-link" href="/administrators" current={nav.path === '/administrators'}>{t('nav.administrators')}</Link>
        {#if me.scope === 'platform'}<Link class="nav-link" href="/plans" current={nav.path === '/plans'}>{t('nav.plans')}</Link>{/if}
      </div>
      {#if me.scope === 'platform'}
        <div class="nav-group">
          <h2>{t('nav.monitoring')}</h2>
          <Link class="nav-link" href="/monitor" current={nav.path === '/monitor'}>{t('nav.monitor')}</Link>
          <Link class="nav-link" href="/protocols" current={nav.path === '/protocols'}>{t('nav.protocols')}</Link>
        </div>
      {/if}
    </nav>
  </aside>
  <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
  <div class="scrim" class:open={menuOpen} onclick={() => (menuOpen = false)}></div>

  <div class="main-col">
    <header class="top">
      <div class="crumbs">
        <button type="button" class="menu-button" aria-expanded={menuOpen} aria-controls="sidebar" onclick={() => (menuOpen = !menuOpen)}>{t('nav.menu')}</button>
        {#if crumbs.length > 0}
          <nav aria-label={t('nav.breadcrumb')}>
            <ol>
              {#each crumbs as c, i (i)}
                <li>
                  {#if i === crumbs.length - 1}<span aria-current="page">{c.label}</span>{:else if c.to !== undefined}<Link href={c.to}>{c.label}</Link>{:else}{c.label}{/if}
                </li>
              {/each}
            </ol>
          </nav>
        {/if}
      </div>
      <div class="who">
        {#if app.live.connected}<span class="live" title={t('nav.liveHint')}>{t('nav.live')}</span>{/if}
        <div class="who-text">
          <div class="who-address">{me.address}</div>
          <div class="who-scope">{me.scope === 'platform' ? t('home.scopePlatform') : t('home.scopeDomain')}</div>
        </div>
        <button type="button" onclick={() => app.signOut()}>{t('nav.signOut')}</button>
      </div>
    </header>

    {#if match === null}
      <NotFound />
    {:else if match.value === 'domains'}
      <Domains {app} {me} />
    {:else if match.value === 'administrators'}
      <Administrators {app} {me} />
    {:else if match.value === 'monitor'}
      <!-- Only for platform administrators: for anyone else the page does not exist (the API says 403 as well). -->
      {#if me.scope === 'platform'}<MonitorPage {app} />{:else}<NotFound />{/if}
    {:else if match.value === 'plans'}
      {#if me.scope === 'platform'}<PlansPage {app} />{:else}<NotFound />{/if}
    {:else if match.value === 'protocols'}
      {#if me.scope === 'platform'}<ProtocolsPage {app} />{:else}<NotFound />{/if}
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
  </div>
</div>
