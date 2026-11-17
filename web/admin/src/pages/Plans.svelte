<script lang="ts">
  import PageHead from '../lib/PageHead.svelte';
  import { onMount } from 'svelte';
  import type { App } from '../lib/app.svelte';
  import type { Plan } from '../api/types';
  import { Load } from '../lib/load.svelte';
  import ErrorNote from '../lib/ErrorNote.svelte';
  import { labelled } from '../lib/stack';
  import { bytes, megabytes } from '../lib/bytes';
  import { t } from '../i18n';

  let { app }: { app: App } = $props();

  const load = new Load<Plan[]>(() => app.client.plans());
  let busy = $state(false);
  let error = $state<unknown>(null);
  let notice = $state<string | null>(null);
  let invalid = $state(false);

  // The form makes a plan or changes the one of the same name; an empty limit means "none of its own".
  let name = $state('');
  let quota = $state('');
  let maxMessage = $state('');

  onMount(() => void load.run());

  const edit = (p: Plan): void => {
    name = p.name;
    quota = p.quota_bytes === null ? '' : String(p.quota_bytes / 1_048_576);
    maxMessage = p.max_message_bytes === null ? '' : String(p.max_message_bytes / 1_048_576);
    notice = null;
    invalid = false;
  };

  async function save(e: SubmitEvent): Promise<void> {
    e.preventDefault();
    const q = megabytes(quota);
    const m = megabytes(maxMessage);
    invalid = Number.isNaN(q) || Number.isNaN(m);
    if (invalid) return;
    busy = true;
    error = null;
    notice = null;
    try {
      const saved = await app.client.putPlan(name.trim(), q, m);
      notice = t('plans.saved', { name: saved.name });
      name = '';
      quota = '';
      maxMessage = '';
      await load.run();
    } catch (err) {
      error = err;
    } finally {
      busy = false;
    }
  }

  const limit = (n: number | null): string => (n === null ? t('plans.none') : bytes(n));
</script>

<main id="content" tabindex="-1">
  <PageHead title={t('plans.title')} lede={t('plans.lede')} />
  {#if error !== null}<ErrorNote {error} />{/if}
  {#if load.state.status === 'error'}<ErrorNote error={load.state.error} />{/if}
  {#if invalid}<p class="alert danger" role="alert">{t('plans.invalid')}</p>{/if}
  {#if notice !== null}<p class="alert ok" role="status">{notice}</p>{/if}
  {#if load.state.status === 'loading'}
    <p class="muted" role="status">{t('app.loading')}</p>
  {:else if load.state.status === 'ready'}
    {#if load.state.data.length === 0}
      <div class="empty"><p>{t('plans.empty')}</p></div>
    {:else}
      <div class="table-wrap">
        <table use:labelled aria-label={t('plans.title')}>
          <thead>
            <tr><th scope="col">{t('plans.col.name')}</th><th scope="col">{t('plans.col.quota')}</th><th scope="col">{t('plans.col.maxMessage')}</th><th scope="col" class="col-actions">{t('plans.col.action')}</th></tr>
          </thead>
          <tbody>
            {#each load.state.data as p (p.name)}
              <tr>
                <th scope="row">{p.name}</th>
                <td>{limit(p.quota_bytes)}</td>
                <td>{limit(p.max_message_bytes)}</td>
                <td class="col-actions"><button type="button" class="secondary" onclick={() => edit(p)}>{t('plans.edit')}</button></td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
    {/if}
    <section class="panel" aria-labelledby="plan-form">
      <h2 id="plan-form">{t('plans.form')}</h2>
      <form onsubmit={save}>
        <div class="field-row">
          <label>{t('plans.col.name')}<input bind:value={name} name="name" type="text" autocapitalize="none" spellcheck="false" required /></label>
          <label>{t('plans.quotaMb')}<input bind:value={quota} name="quota" type="text" inputmode="decimal" /></label>
          <label>{t('plans.maxMessageMb')}<input bind:value={maxMessage} name="max" type="text" inputmode="decimal" /></label>
          <button type="submit" class="primary" disabled={busy}>{busy ? t('common.saving') : t('common.save')}</button>
        </div>
        <p class="muted">{t('plans.note')}</p>
      </form>
    </section>
  {/if}
</main>
