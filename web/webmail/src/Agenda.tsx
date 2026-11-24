import { createResource, For, Show, type Component } from 'solid-js';
import type { Api, AgendaItem } from './api';
import { errorText } from './errors';
import { dayHeading, eventClock, eventDay } from './format';
import { t } from './i18n';

const DAYS = 14;

/** The events of the next two weeks, by day (kept through a calendar program or phone; this page only reads them). */
export const Agenda: Component<{ api: Api }> = (props) => {
  const [agenda] = createResource(() => {
    const start = new Date();
    start.setHours(0, 0, 0, 0);
    const from = Math.floor(start.getTime() / 1000);
    return props.api.events(from, from + DAYS * 86400);
  });
  const days = () => {
    const by = new Map<string, AgendaItem[]>();
    for (const e of agenda()?.items ?? []) {
      const day = eventDay(e);
      by.set(day, [...(by.get(day) ?? []), e]);
    }
    return [...by.entries()].sort(([a], [b]) => (a < b ? -1 : 1));
  };
  return (
    <section class="agenda" aria-label={t('agenda.title')}>
      <h2>{t('agenda.title')}</h2>
      <Show when={!agenda.error} fallback={<p class="error">{errorText(agenda.error)}</p>}>
        <Show when={days().length > 0} fallback={<p class="empty">{t('agenda.empty')}</p>}>
          <For each={days()}>
            {([day, events]) => (
              <div class="day">
                <h3>{dayHeading(day)}</h3>
                <ul>
                  <For each={events}>
                    {(e) => (
                      <li>
                        <span class="clock">{e.all_day ? t('agenda.allday') : eventClock(e)}</span>
                        <strong>{e.summary}</strong>
                        <Show when={e.location !== ''}>
                          <span class="where">{e.location}</span>
                        </Show>
                        <Show when={!e.utc && !e.all_day && e.tzid !== null}>
                          <span class="zone">{e.tzid}</span>
                        </Show>
                        <Show when={e.unexpanded}>
                          <span class="note">{t('agenda.repeats')}</span>
                        </Show>
                      </li>
                    )}
                  </For>
                </ul>
              </div>
            )}
          </For>
        </Show>
        <Show when={agenda()?.truncated}>
          <p class="note">{t('agenda.more')}</p>
        </Show>
      </Show>
    </section>
  );
};
