import { createResource, For, Show, type Component } from 'solid-js';
import type { Api } from './api';
import { errorText } from './errors';
import { when } from './format';
import { t, tFor } from './i18n';

/** What was sent lately and how it went. A message that is still waiting, or did not go, can be withdrawn. */
export const Outbox: Component<{ api: Api }> = (props) => {
  const [items, { refetch }] = createResource(() => props.api.outbox());
  const withdraw = async (id: number) => {
    await props.api.withdraw(id);
    await refetch();
  };
  return (
    <section class="outbox" aria-label={t('outbox.title')}>
      <h2>{t('outbox.title')}</h2>
      <Show when={!items.error} fallback={<p class="error">{errorText(items.error)}</p>}>
        <Show when={(items() ?? []).length > 0} fallback={<p class="empty">{t('outbox.empty')}</p>}>
          <ul>
            <For each={items()}>
              {(i) => (
                <li class={`state-${i.state}`}>
                  <div class="line">
                    <strong>{i.subject === '' ? t('list.nosubject') : i.subject}</strong>
                    <span class="when">{when(i.created)}</span>
                  </div>
                  <div class="to">{i.recipients.join(', ')}</div>
                  <div class="line">
                    <span class="badge">{tFor('outbox.state.', i.state, 'error.unknown')}</span>
                    <Show when={i.error}>{(why) => <span class="why">{tFor('outbox.why.', why(), 'outbox.why.failed')}</span>}</Show>
                    <Show when={i.state === 'queued' || i.state === 'failed'}>
                      <button type="button" onClick={() => void withdraw(i.id)}>
                        {t('outbox.withdraw')}
                      </button>
                    </Show>
                  </div>
                </li>
              )}
            </For>
          </ul>
        </Show>
      </Show>
    </section>
  );
};
