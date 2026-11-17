// Live updates (docs/design-live-updates.md): one EventSource for the whole console. The stream carries hints, never data: a hint names a topic that changed, and a page
// that shows that topic asks the API again. So a page says what it shows (`watch(['storage:example.test'], refresh)`) and nothing else; a burst of hints is merged into one
// refresh, `resync` (sent at every connection, so also after a reconnection) refreshes every page, and without a stream the console works as it always did.

/** What the console needs of an event source: the browser's `EventSource` or a test's fake. */
export interface Source {
  addEventListener(type: string, listener: (e: MessageEvent<string>) => void): void;
  onopen: ((e: Event) => unknown) | null;
  onerror: ((e: Event) => unknown) | null;
  close(): void;
}

export type SourceFactory = (url: string) => Source;

const browser: SourceFactory = (url) => new EventSource(url);

/** A burst of hints is answered by one refresh, this long after the first. */
export const MERGE_MS = 250;

interface Watcher {
  topics: string[];
  run: () => void;
  timer: ReturnType<typeof setTimeout> | null;
}

export class Live {
  /** Whether the stream is connected now (the top bar says so). */
  connected = $state(false);

  private readonly make: SourceFactory;
  private source: Source | null = null;
  private watchers: Watcher[] = [];

  constructor(make: SourceFactory | null = typeof EventSource === 'undefined' ? null : browser) {
    this.make = make ?? (() => ({ addEventListener() {}, onopen: null, onerror: null, close() {} }));
  }

  /** Open the stream (once). The browser reconnects by itself when it drops. */
  start(): void {
    if (this.source !== null) return;
    const s = this.make('/api/v1/events');
    this.source = s;
    s.onopen = () => {
      this.connected = true;
    };
    s.onerror = () => {
      this.connected = false;
    };
    s.addEventListener('resync', () => {
      for (const w of this.watchers) this.schedule(w);
    });
    s.addEventListener('changed', (e) => {
      const topics = parse(e.data);
      for (const w of this.watchers) if (topics.some((t) => w.topics.includes(t))) this.schedule(w);
    });
  }

  stop(): void {
    this.source?.close();
    this.source = null;
    this.connected = false;
    for (const w of this.watchers) if (w.timer !== null) clearTimeout(w.timer);
  }

  /** Run `run` when one of `topics` changed (and after a resync). Returns the function that stops watching. */
  watch(topics: string[], run: () => void): () => void {
    const w: Watcher = { topics, run, timer: null };
    this.watchers.push(w);
    return () => {
      if (w.timer !== null) clearTimeout(w.timer);
      this.watchers = this.watchers.filter((x) => x !== w);
    };
  }

  private schedule(w: Watcher): void {
    if (w.timer !== null) return;
    w.timer = setTimeout(() => {
      w.timer = null;
      w.run();
    }, MERGE_MS);
  }
}

function parse(data: string): string[] {
  try {
    const v: unknown = JSON.parse(data);
    const topics = (v as { topics?: unknown }).topics;
    return Array.isArray(topics) ? topics.filter((t): t is string => typeof t === 'string') : [];
  } catch {
    return [];
  }
}
