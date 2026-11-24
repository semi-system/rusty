/** A time the way a mail list shows it: the hour for today, the day for this year, the year for older mail. */
export function when(unix: number, now: Date = new Date()): string {
  const d = new Date(unix * 1000);
  if (d.toDateString() === now.toDateString()) {
    return d.toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit' });
  }
  if (d.getFullYear() === now.getFullYear()) {
    return d.toLocaleDateString(undefined, { day: 'numeric', month: 'short' });
  }
  return d.toLocaleDateString(undefined, { year: 'numeric', month: 'short', day: 'numeric' });
}

export function size(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(bytes < 10240 ? 1 : 0)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

const pad = (n: number) => String(n).padStart(2, '0');

/** The day (`YYYY-MM-DD`) an agenda event starts on: in the reader's zone for a UTC time, as written for any other. */
export function eventDay(e: { start: number; utc: boolean }): string {
  const d = new Date(e.start * 1000);
  return e.utc ? `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}` : d.toISOString().slice(0, 10);
}

/** The hour and minute an agenda event starts at (empty for a whole-day event). */
export function eventClock(e: { start: number; utc: boolean; all_day: boolean }): string {
  if (e.all_day) return '';
  const d = new Date(e.start * 1000);
  return e.utc ? `${pad(d.getHours())}:${pad(d.getMinutes())}` : d.toISOString().slice(11, 16);
}

/** A day heading of the agenda from its `YYYY-MM-DD`. */
export function dayHeading(day: string): string {
  return new Date(`${day}T00:00:00Z`).toLocaleDateString(undefined, { weekday: 'long', day: 'numeric', month: 'long', timeZone: 'UTC' });
}
