<script lang="ts">
  import { onMount } from 'svelte';
  import type { App } from '../lib/app.svelte';
  import type { Certificate } from '../api/types';
  import Badge from '../lib/Badge.svelte';
  import ErrorNote from '../lib/ErrorNote.svelte';
  import PageHead from '../lib/PageHead.svelte';
  import { labelled } from '../lib/stack';
  import { tFor, t } from '../i18n';

  let { app }: { app: App } = $props();

  const day = (s: number): string => new Intl.DateTimeFormat(undefined, { dateStyle: 'medium', timeZone: 'UTC' }).format(new Date(s * 1000));

  let items = $state<Certificate[]>([]);
  let loaded = $state(false);
  let error = $state<unknown>(null);

  async function load(): Promise<void> {
    loaded = false;
    error = null;
    try {
      items = (await app.client.certificates()).items;
    } catch (e) {
      error = e;
    }
    loaded = true;
  }

  onMount(() => void load());

  // Under two weeks is a warning, expired or unreadable is a failure.
  const tone = (c: Certificate): 'ok' | 'warn' | 'danger' => (c.days_left === null || c.days_left < 0 ? 'danger' : c.days_left < 14 ? 'warn' : 'ok');
</script>

<main id="content" tabindex="-1">
  <PageHead title={t('certs.title')} lede={t('certs.lede')} />
  {#if error !== null}<ErrorNote {error} />{/if}
  <p><button type="button" class="secondary" onclick={() => void load()}>{t('certs.check')}</button></p>
  {#if !loaded}
    <p class="muted" role="status">{t('certs.checking')}</p>
  {:else if items.length > 0}
    <div class="table-wrap">
      <table use:labelled aria-label={t('certs.title')}>
        <thead>
          <tr>
            <th scope="col">{t('certs.col.name')}</th>
            <th scope="col">{t('certs.col.use')}</th>
            <th scope="col">{t('certs.col.subject')}</th>
            <th scope="col">{t('certs.col.issuer')}</th>
            <th scope="col">{t('certs.col.until')}</th>
            <th scope="col">{t('certs.col.left')}</th>
          </tr>
        </thead>
        <tbody>
          {#each items as c (c.host + ':' + c.port)}
            <tr>
              <th scope="row">{c.host}:{c.port}</th>
              <td>{tFor('certs.use.', c.purpose, 'certs.use.web')}</td>
              {#if c.error !== null}
                <td colspan="4"><Badge tone="danger">{t('certs.failed')}</Badge> {c.error}</td>
              {:else}
                <td>{c.subject}</td>
                <td>{c.issuer}</td>
                <td>{c.not_after === null ? '' : day(c.not_after)}</td>
                <td><Badge tone={tone(c)}>{t('certs.days', { n: c.days_left ?? 0 })}</Badge></td>
              {/if}
            </tr>
          {/each}
        </tbody>
      </table>
    </div>
  {/if}
</main>
