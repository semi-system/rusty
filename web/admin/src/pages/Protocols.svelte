<script lang="ts">
  import PageHead from '../lib/PageHead.svelte';
  import Badge from '../lib/Badge.svelte';
  import { onMount } from 'svelte';
  import type { App } from '../lib/app.svelte';
  import type { Protocol } from '../api/types';
  import { Load } from '../lib/load.svelte';
  import ErrorNote from '../lib/ErrorNote.svelte';
  import { labelled } from '../lib/stack';
  import { t } from '../i18n';

  let { app }: { app: App } = $props();

  const load = new Load<{ protocols: Protocol[] }>(() => app.client.protocols());
  let busy = $state<string | null>(null);
  let error = $state<unknown>(null);

  onMount(() => void load.run());

  async function flip(p: Protocol): Promise<void> {
    busy = p.name;
    error = null;
    try {
      await app.client.setProtocol(p.name, !p.enabled);
      await load.run();
    } catch (e) {
      error = e;
    } finally {
      busy = null;
    }
  }

  /** The words for a protocol: a catalogue key per name, the name itself for one this page does not know yet. */
  function label(name: string): string {
    return name === 'pop3' ? t('protocols.pop3') : name;
  }
</script>

<main id="content" tabindex="-1">
  <PageHead title={t('protocols.title')} lede={t('protocols.lede')} />
  {#if error}<ErrorNote {error} />{/if}
  {#if load.state.status === 'error'}<ErrorNote error={load.state.error} />{/if}
  {#if load.state.status === 'loading'}
    <p class="muted" role="status">{t('app.loading')}</p>
  {:else if load.state.status === 'ready'}
    <div class="table-wrap">
      <table use:labelled aria-label={t('protocols.title')}>
        <thead>
          <tr><th scope="col">{t('protocols.col.protocol')}</th><th scope="col">{t('protocols.col.state')}</th><th scope="col">{t('protocols.col.action')}</th></tr>
        </thead>
        <tbody>
          {#each load.state.data.protocols as p (p.name)}
            <tr>
              <td>{label(p.name)}</td>
              <td><Badge tone={p.enabled ? 'ok' : 'neutral'}>{p.enabled ? t('protocols.on') : t('protocols.off')}</Badge></td>
              <td>
                <button type="button" class="secondary" disabled={busy === p.name} onclick={() => flip(p)}>{p.enabled ? t('protocols.turnOff') : t('protocols.turnOn')}</button>
              </td>
            </tr>
          {/each}
        </tbody>
      </table>
    </div>
    <p class="muted">{t('protocols.note')}</p>
  {/if}
</main>
