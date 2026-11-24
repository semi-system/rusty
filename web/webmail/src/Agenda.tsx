import { createResource, createSignal, For, Show, type Component } from 'solid-js';
import type { Api, AgendaItem } from './api';
import { errorText } from './errors';
import { dayHeading, eventClock, eventDay } from './format';
import { t } from './i18n';

const DAYS = 14;

/** The UTC seconds of a day and an hour of the reader's own clock (`2030-05-06`, `09:30`). */
function localSecs(day: string, clock: string): number {
  return Math.floor(new Date(`${day}T${clock}`).getTime() / 1000);
}

/** The events of the next two weeks, by day, and a form to add one. Events are kept through a calendar program or phone as well (CalDAV); this page lists them and adds single events. */
export const Agenda: Component<{ api: Api }> = (props) => {
  const [agenda, { refetch }] = createResource(() => {
    const start = new Date();
    start.setHours(0, 0, 0, 0);
    const from = Math.floor(start.getTime() / 1000);
    return props.api.events(from, from + DAYS * 86400);
  });
  const [summary, setSummary] = createSignal('');
  const [place, setPlace] = createSignal('');
  const [day, setDay] = createSignal('');
  const [from, setFrom] = createSignal('09:00');
  const [until, setUntil] = createSignal('10:00');
  const [wholeDay, setWholeDay] = createSignal(false);
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal('');
  const add = async (e: SubmitEvent) => {
    e.preventDefault();
    setError('');
    if (day() === '') {
      setError(t('error.invalid'));
      return;
    }
    // A whole-day event is the UTC midnights around its date; any other is the reader's clock turned into UTC.
    const midnight = Math.floor(Date.parse(`${day()}T00:00:00Z`) / 1000);
    const [start, end] = wholeDay() ? [midnight, midnight + 86400] : [localSecs(day(), from()), localSecs(day(), until())];
    setBusy(true);
    try {
      await props.api.createEvent({ summary: summary(), location: place(), start, end, all_day: wholeDay() });
      setSummary('');
      setPlace('');
      await refetch();
    } catch (err) {
      setError(errorText(err));
    } finally {
      setBusy(false);
    }
  };
  const days = () => {
    const by = new Map<string, AgendaItem[]>();
    for (const e of agenda()?.items ?? []) {
      const d = eventDay(e);
      by.set(d, [...(by.get(d) ?? []), e]);
    }
    return [...by.entries()].sort(([a], [b]) => (a < b ? -1 : 1));
  };
  return (
    <section class="agenda" aria-label={t('agenda.title')}>
      <h2>{t('agenda.title')}</h2>
      <Show when={!agenda.error} fallback={<p class="error">{errorText(agenda.error)}</p>}>
        <Show when={days().length > 0} fallback={<p class="empty">{t('agenda.empty')}</p>}>
          <For each={days()}>
            {([d, events]) => (
              <div class="day">
                <h3>{dayHeading(d)}</h3>
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
      <h3>{t('agenda.add')}</h3>
      <form class="add-event" onSubmit={add}>
        <label>
          <span>{t('agenda.what')}</span>
          <input name="summary" maxlength="200" required value={summary()} onInput={(e) => setSummary(e.currentTarget.value)} />
        </label>
        <label>
          <span>{t('agenda.where')}</span>
          <input name="location" maxlength="200" value={place()} onInput={(e) => setPlace(e.currentTarget.value)} />
        </label>
        <label>
          <span>{t('agenda.date')}</span>
          <input name="day" type="date" required value={day()} onInput={(e) => setDay(e.currentTarget.value)} />
        </label>
        <label>
          <input name="whole" type="checkbox" checked={wholeDay()} onChange={(e) => setWholeDay(e.currentTarget.checked)} />
          <span>{t('agenda.wholeDay')}</span>
        </label>
        <Show when={!wholeDay()}>
          <label>
            <span>{t('agenda.from')}</span>
            <input name="from" type="time" required value={from()} onInput={(e) => setFrom(e.currentTarget.value)} />
          </label>
          <label>
            <span>{t('agenda.until')}</span>
            <input name="until" type="time" required value={until()} onInput={(e) => setUntil(e.currentTarget.value)} />
          </label>
        </Show>
        <Show when={error() !== ''}>
          <p class="error" role="alert">
            {error()}
          </p>
        </Show>
        <button type="submit" class="primary" disabled={busy()}>
          {t('agenda.save')}
        </button>
      </form>
    </section>
  );
};
