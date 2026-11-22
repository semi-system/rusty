<script lang="ts">
  import Badge from '../lib/Badge.svelte';
  import { onMount } from 'svelte';
  import type { App } from '../lib/app.svelte';
  import type { Dkim } from '../api/types';
  import Copy from '../lib/Copy.svelte';
  import DomainShell from '../lib/DomainShell.svelte';
  import ErrorNote from '../lib/ErrorNote.svelte';
  import { t } from '../i18n';

  let { app, name }: { app: App; name: string } = $props();

  let items = $state<Dkim[]>([]);
  let loaded = $state(false);
  let error = $state<unknown>(null);
  let notice = $state<string | null>(null);
  let busy = $state(false);
  let selector = $state('');
  let retiring = $state<string | null>(null);

  const when = (s: number): string => new Intl.DateTimeFormat(undefined, { dateStyle: 'medium' }).format(new Date(s * 1000));

  // A new selector named after today, so that two rotations never meet on one name.
  const rotationName = (): string => 'r' + new Date().toISOString().slice(0, 10).replaceAll('-', '');

  async function retire(k: Dkim): Promise<void> {
    busy = true;
    error = null;
    notice = null;
    try {
      await app.client.deleteDkim(name, k.selector);
      notice = t('dkim.retired', { selector: k.selector });
      retiring = null;
      await load();
    } catch (err) {
      error = err;
    } finally {
      busy = false;
    }
  }

  async function rotate(): Promise<void> {
    selector = rotationName();
    await make(new SubmitEvent('submit'));
  }

  async function load(quiet = false): Promise<void> {
    try {
      items = await app.client.dkim(name);
    } catch (e) {
      if (!quiet) error = e;
    }
    loaded = true;
  }

  onMount(() => {
    void load();
    // A hint refreshes what is shown in the background, without touching what the person is typing in a form.
    return app.live.watch(['dns:' + name.toLowerCase()], () => void load(true));
  });

  async function make(e: SubmitEvent): Promise<void> {
    e.preventDefault();
    busy = true;
    error = null;
    notice = null;
    try {
      const k = await app.client.createDkim(name, selector);
      notice = t('dkim.created', { selector: k.selector });
      selector = '';
      await load();
    } catch (err) {
      error = err;
    } finally {
      busy = false;
    }
  }
</script>

<main id="content" tabindex="-1">
  <DomainShell {name}>
    <div class="section-head">
      <h2>{t('dkim.title', { domain: name })}</h2>
      <p>{t('dkim.lede')}</p>
    </div>
    <p class="muted">{t('dkim.intro')}</p>
    {#if error !== null}<ErrorNote {error} />{/if}
    {#if notice !== null}<p class="alert ok" role="status">{notice}</p>{/if}

    {#if !loaded}
      <p class="muted" role="status">{t('app.loading')}</p>
    {:else if items.length === 0}
      <div class="empty"><p>{t('dkim.empty')}</p></div>
    {:else}
      <ul aria-label={t('dkim.list')} class="plain">
        {#each items as k (k.selector)}
          <li class="panel">
            <div class="page-title-row">
              <h3>{k.selector}</h3>
              <Badge tone={k.sealed ? 'ok' : 'warn'}>{k.sealed ? t('dkim.sealed') : t('dkim.notSealed')}</Badge>
            </div>
            <p>{t('dkim.recordName')} <Copy value={k.record_name} label={t('dkim.recordName')} /></p>
            <p>{t('dkim.recordValue')} <Copy value={k.record_value} label={t('dkim.recordValue')} /></p>
            <p class="muted">{t('dkim.madeOn', { date: when(k.created_at) })}</p>
            {#if items.length > 1}
              <p><button type="button" class="secondary" disabled={busy} aria-label={t('dkim.retireFor', { selector: k.selector })} onclick={() => (retiring = retiring === k.selector ? null : k.selector)}>{t('dkim.retire')}</button></p>
              {#if retiring === k.selector}
                <div class="row-form" role="alert">
                  <p>{t('dkim.retireAsk')}</p>
                  <div class="form-actions">
                    <button type="button" class="danger" disabled={busy} onclick={() => retire(k)}>{t('dkim.retireYes')}</button>
                    <button type="button" class="secondary" onclick={() => (retiring = null)}>{t('accounts.cancel')}</button>
                  </div>
                </div>
              {/if}
            {/if}
          </li>
        {/each}
      </ul>
    {/if}

    <section class="panel" aria-labelledby="new-dkim">
      <h2 id="new-dkim">{t('dkim.new')}</h2>
      <form onsubmit={make}>
        <div class="field-row">
          <label>
            {t('dkim.newSelector')}
            <input bind:value={selector} name="selector" type="text" autocapitalize="none" spellcheck="false" />
          </label>
          <button type="submit" disabled={busy}>{busy ? t('common.creating') : t('dkim.new')}</button>
          <button type="button" class="secondary" disabled={busy} onclick={rotate}>{t('dkim.rotate')}</button>
        </div>
        <p class="hint">{t('dkim.rotateHelp')}</p>
      </form>
    </section>
  </DomainShell>
</main>
