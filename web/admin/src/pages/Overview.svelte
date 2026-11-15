<script lang="ts">
  import { onMount } from 'svelte';
  import type { App } from '../lib/app.svelte';
  import { ApiError } from '../api/client';
  import type { Domain, Me, Queue } from '../api/types';
  import DomainShell from '../lib/DomainShell.svelte';
  import ErrorNote from '../lib/ErrorNote.svelte';
  import { t, tFor } from '../i18n';

  let { app, me, name }: { app: App; me: Me; name: string } = $props();

  let domain = $state<Domain | null>(null);
  let queue = $state<Queue | null>(null);
  let error = $state<unknown>(null);
  let busy = $state(false);

  onMount(async () => {
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
    } catch (e) {
      error = e;
    }
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
</script>

<main id="content" tabindex="-1">
  <DomainShell {name}>
    {#if error !== null}<ErrorNote {error} />{/if}
    {#if domain !== null}
      <p>{tFor('domain.status.', domain.status, 'domain.status.active')}</p>
      {#if domain.status !== 'active'}<p class="note">{t('domain.disabledNote')}</p>{/if}
      {#if me.scope === 'platform'}
        <button type="button" onclick={toggle} disabled={busy}>{domain.status === 'active' ? t('domain.disable') : t('domain.enable')}</button>
      {/if}
    {/if}
    {#if queue !== null}
      <h2>{t('domain.queue')}</h2>
      <ul>
        <li>{t('domain.queueQueued', { n: queue.queued })}</li>
        <li>{t('domain.queueRunning', { n: queue.running })}</li>
        <li>{t('domain.queueFailed', { n: queue.failed })}</li>
        <li>{t('domain.queueDead', { n: queue.dead })}</li>
      </ul>
    {/if}
  </DomainShell>
</main>
