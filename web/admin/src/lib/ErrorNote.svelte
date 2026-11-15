<script lang="ts">
  import { errorMessage } from '../api/client';
  import { t } from '../i18n';

  let { error }: { error: unknown } = $props();
  const m = $derived(errorMessage(error));
</script>

<p class="note" role="alert">{t(m.key, m.params)}</p>
