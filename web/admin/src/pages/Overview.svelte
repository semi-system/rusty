<script lang="ts">
  import { onMount } from 'svelte';
  import type { App } from '../lib/app.svelte';
  import { ApiError } from '../api/client';
  import type { DnsRecord, Domain, Me, Queue } from '../api/types';
  import Badge from '../lib/Badge.svelte';
  import DomainShell from '../lib/DomainShell.svelte';
  import ErrorNote from '../lib/ErrorNote.svelte';
  import Link from '../lib/Link.svelte';
  import { href } from '../lib/router';
  import { t, tFor } from '../i18n';

  let { app, me, name }: { app: App; me: Me; name: string } = $props();

  let domain = $state<Domain | null>(null);
  let queue = $state<Queue | null>(null);
  let records = $state<DnsRecord[] | null>(null);
  let error = $state<unknown>(null);
  let busy = $state(false);

  async function load(quiet = false): Promise<void> {
    try {
      // The list holds the domains this administrator may see; the one named here is among them or it is not found.
      let after: number | undefined;
      for (;;) {
        const page = await app.client.domains(after);
        const found = page.items.find((d) => d.name === name);
        if (found) {
          domain = found;
          break;
        }
        if (page.next === null) throw new ApiError(404, 'not_found');
        after = page.next;
      }
      queue = await app.client.outbound(name);
      error = null;
    } catch (e) {
      if (!quiet) error = e;
      return;
    }
    // The progress of the setup is a convenience: when the records cannot be read the card says nothing rather than failing the page.
    try {
      records = (await app.client.dns(name)).records;
    } catch {
      if (!quiet) records = null;
    }
  }

  onMount(() => {
    void load();
    // The status of the domain, its queue and its DNS records refresh in the background when a hint says one of them changed.
    return app.live.watch(['domains', 'outbound:' + name.toLowerCase(), 'dns:' + name.toLowerCase()], () => void load(true));
  });

  async function toggle(): Promise<void> {
    if (domain === null) return;
    busy = true;
    error = null;
    try {
      domain = await app.client.setDomainEnabled(name, domain.status !== 'active');
    } catch (e) {
      error = e;
    } finally {
      busy = false;
    }
  }

  // The records the domain cannot work without, and how many of them the last check found correct.
  const required = $derived((records ?? []).filter((r) => r.required === 'required'));
  const verified = $derived(required.filter((r) => r.status === 'ok' || r.status === 'warning').length);
  const checked = $derived(required.some((r) => r.status !== 'unchecked'));
  // The bar is drawn in steps of ten per cent: a class, because the page may not carry inline styles.
  const step = $derived(required.length === 0 ? 0 : Math.round((verified / required.length) * 10) * 10);
</script>

<main id="content" tabindex="-1">
  <DomainShell {name}>
    {#if error !== null}<ErrorNote {error} />{/if}
    {#if domain !== null}
      <section aria-labelledby="ov-status">
        <div class="section-head">
          <h2 id="ov-status">{t('domains.statusHeading')}</h2>
        </div>
        <div class="panel">
          <div class="inline-row">
            <Badge tone={domain.status === 'active' ? 'ok' : 'warn'}>{tFor('domain.status.', domain.status, 'domain.status.active')}</Badge>
            <span class="muted">{t('domain.statusLede')}</span>
            {#if me.scope === 'platform'}
              <button type="button" class={domain.status === 'active' ? 'secondary' : ''} onclick={toggle} disabled={busy}>{domain.status === 'active' ? t('domain.disable') : t('domain.enable')}</button>
            {/if}
          </div>
          {#if domain.status !== 'active'}<p class="alert warn" role="status">{t('domain.disabledNote')}</p>{/if}
        </div>
      </section>

      {#if records !== null && required.length > 0}
        <section aria-labelledby="ov-setup">
          <div class="section-head">
            <h2 id="ov-setup">{t('domain.setup')}</h2>
          </div>
          <div class="panel progress">
            <ul class="checklist">
              <li>
                <Badge tone={checked ? (verified === required.length ? 'ok' : 'warn') : 'neutral'}>{checked ? t('domain.setupDns', { done: verified, total: required.length }) : t('domain.setupDnsNone')}</Badge>
                <Link href={href('/domains/{name}/dns', { name })}>{t('domain.setupDnsAction')}</Link>
              </li>
            </ul>
            <div class="progress-bar" role="presentation"><div class={'progress-fill p' + step}></div></div>
          </div>
        </section>
      {/if}
    {/if}

    {#if queue !== null}
      <section aria-labelledby="ov-queue">
        <div class="section-head">
          <h2 id="ov-queue">{t('domain.queue')}</h2>
          <p>{t('domain.queueLede')}</p>
        </div>
        <div class="stats">
          <div class="stat"><div class="stat-value">{queue.queued}</div><div class="stat-label">{t('domain.queueQueued')}</div></div>
          <div class="stat"><div class="stat-value">{queue.running}</div><div class="stat-label">{t('domain.queueRunning')}</div></div>
          <div class={queue.failed > 0 ? 'stat warn' : 'stat'}><div class="stat-value">{queue.failed}</div><div class="stat-label">{t('domain.queueFailed')}</div></div>
          <div class={queue.dead > 0 ? 'stat danger' : 'stat'}><div class="stat-value">{queue.dead}</div><div class="stat-label">{t('domain.queueDead')}</div></div>
        </div>
      </section>
    {/if}
  </DomainShell>
</main>
