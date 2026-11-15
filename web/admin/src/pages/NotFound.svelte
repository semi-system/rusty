<script lang="ts">
  import Link from '../lib/Link.svelte';
  import { t } from '../i18n';
</script>

<main id="content" tabindex="-1">
  <h1>{t('notfound.title')}</h1>
  <p class="muted">{t('notfound.body')}</p>
  <p><Link href="/">{t('nav.domains')}</Link></p>
</main>
