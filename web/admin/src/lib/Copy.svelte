<script lang="ts">
  import { t } from '../i18n';

  // A value to publish, always shown in a read-only field (so it can be selected and copied by hand when the clipboard is not available) with a button
  // that copies it. `label` names the field for a screen reader.
  let { value, label }: { value: string; label: string } = $props();
  let copied = $state(false);
  let field: HTMLInputElement | undefined = $state();

  async function copy(): Promise<void> {
    try {
      await navigator.clipboard.writeText(value);
      copied = true;
    } catch {
      // No clipboard permission or API: select the text so that the person can copy it.
      field?.select();
      copied = false;
    }
  }
</script>

<span class="copy">
  <input bind:this={field} readonly {value} aria-label={label} onfocus={(e) => e.currentTarget.select()} />
  <button type="button" onclick={copy}>{copied ? t('common.copied') : t('common.copy')}</button>
</span>
