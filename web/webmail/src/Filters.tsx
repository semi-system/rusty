import { createResource, createSignal, For, Show, type Component } from 'solid-js';
import { ApiError, type Api } from './api';
import { errorText } from './errors';
import { t } from './i18n';

const STARTER = 'require "fileinto";\n\n# Put mail from a list into a folder:\n# if header :contains "list-id" "lists.example.org" {\n#   fileinto "Lists";\n# }\n';

/** The filters of the account, written in Sieve (docs/design-sieve.md): one script is edited at a time, saved only when the server's parser accepts it, and one runs on the incoming mail. */
export const Filters: Component<{ api: Api }> = (props) => {
  const [list, { refetch }] = createResource(() => props.api.scripts());
  const [name, setName] = createSignal('main');
  const [body, setBody] = createSignal(STARTER);
  const [error, setError] = createSignal('');
  const [saved, setSaved] = createSignal(false);
  const guard = async (work: () => Promise<void>) => {
    setError('');
    setSaved(false);
    try {
      await work();
    } catch (e) {
      if (e instanceof ApiError && e.code === 'script_invalid') {
        setError(t('filters.invalid', { line: String(e.detail['line'] ?? '?'), message: String(e.detail['message'] ?? '') }));
      } else {
        setError(errorText(e));
      }
    }
  };
  const open = (n: string) =>
    guard(async () => {
      const s = await props.api.script(n);
      setName(s.name);
      setBody(s.body);
    });
  const save = (e: SubmitEvent) => {
    e.preventDefault();
    void guard(async () => {
      await props.api.saveScript(name().trim(), body());
      setSaved(true);
      await refetch();
    });
  };
  const choose = (n: string | null) =>
    guard(async () => {
      await props.api.activateScript(n);
      await refetch();
    });
  const remove = (n: string) =>
    guard(async () => {
      await props.api.deleteScript(n);
      await refetch();
    });
  return (
    <>
      <h2>{t('filters.title')}</h2>
      <p class="note">{t('filters.about')}</p>
      <Show when={error() !== ''}>
        <p class="error" role="alert">
          {error()}
        </p>
      </Show>
      <Show when={(list()?.scripts ?? []).length > 0}>
        <ul class="apppw" aria-label={t('filters.list')}>
          <For each={list()?.scripts}>
            {(s) => (
              <li>
                <div class="line">
                  <button type="button" class="link" onClick={() => void open(s.name)}>
                    {s.name}
                  </button>
                  <Show when={s.active}>
                    <span class="badge">{t('filters.active')}</span>
                  </Show>
                </div>
                <div class="row">
                  <button type="button" onClick={() => void choose(s.active ? null : s.name)}>
                    {s.active ? t('filters.turnOff') : t('filters.turnOn')}
                  </button>
                  <Show when={!s.active}>
                    <button type="button" class="danger" onClick={() => void remove(s.name)}>
                      {t('filters.delete')}
                    </button>
                  </Show>
                </div>
              </li>
            )}
          </For>
        </ul>
      </Show>
      <form onSubmit={save}>
        <label>
          <span>{t('filters.name')}</span>
          <input name="script-name" maxlength="64" required value={name()} onInput={(e) => setName(e.currentTarget.value)} />
        </label>
        <label>
          <span>{t('filters.script')}</span>
          <textarea name="script" rows="12" spellcheck={false} value={body()} onInput={(e) => setBody(e.currentTarget.value)} />
        </label>
        <button type="submit" class="primary">
          {t('filters.save')}
        </button>
        <Show when={saved()}>
          <span class="note" role="status">
            {t('filters.saved')}
          </span>
        </Show>
      </form>
    </>
  );
};
