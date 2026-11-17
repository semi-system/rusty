import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { Live, MERGE_MS, type Source } from './live.svelte';

/** A stream the test drives by hand. */
class Fake implements Source {
  onopen: ((e: Event) => unknown) | null = null;
  onerror: ((e: Event) => unknown) | null = null;
  closed = false;
  private readonly on = new Map<string, (e: MessageEvent<string>) => void>();
  addEventListener(type: string, listener: (e: MessageEvent<string>) => void): void {
    this.on.set(type, listener);
  }
  close(): void {
    this.closed = true;
  }
  send(type: string, data = '{}'): void {
    this.on.get(type)?.({ data } as MessageEvent<string>);
  }
}

let fake: Fake;
let live: Live;
beforeEach(() => {
  vi.useFakeTimers();
  fake = new Fake();
  live = new Live(() => fake);
  live.start();
});
afterEach(() => {
  live.stop();
  vi.useRealTimers();
});

describe('live updates', () => {
  it('refreshes only the pages that watch a topic that changed, once for a burst', () => {
    const storage = vi.fn();
    const accounts = vi.fn();
    live.watch(['storage:example.test'], storage);
    live.watch(['accounts:example.test'], accounts);
    fake.send('changed', '{"topics":["storage:example.test"]}');
    fake.send('changed', '{"topics":["storage:example.test","dns:example.test"]}');
    fake.send('changed', '{"topics":["storage:example.test"]}');
    expect(storage).not.toHaveBeenCalled();
    vi.advanceTimersByTime(MERGE_MS + 1);
    expect(storage).toHaveBeenCalledTimes(1);
    expect(accounts).not.toHaveBeenCalled();
  });

  it('refreshes every watching page on resync, which is what a reconnection starts with', () => {
    const a = vi.fn();
    const b = vi.fn();
    live.watch(['x'], a);
    live.watch(['y'], b);
    fake.send('resync');
    vi.advanceTimersByTime(MERGE_MS + 1);
    expect(a).toHaveBeenCalledTimes(1);
    expect(b).toHaveBeenCalledTimes(1);
  });

  it('stops refreshing a page that stopped watching, and ignores what it cannot read', () => {
    const run = vi.fn();
    const stop = live.watch(['x'], run);
    fake.send('changed', 'not json');
    fake.send('changed', '{"topics":"x"}');
    vi.advanceTimersByTime(MERGE_MS + 1);
    expect(run).not.toHaveBeenCalled();
    fake.send('changed', '{"topics":["x"]}');
    stop();
    vi.advanceTimersByTime(MERGE_MS + 1);
    expect(run).not.toHaveBeenCalled();
  });

  it('says whether it is connected and closes the stream when stopped', () => {
    expect(live.connected).toBe(false);
    fake.onopen?.(new Event('open'));
    expect(live.connected).toBe(true);
    fake.onerror?.(new Event('error'));
    expect(live.connected).toBe(false);
    live.stop();
    expect(fake.closed).toBe(true);
  });

  it('opens one stream however often it is started', () => {
    let made = 0;
    const l = new Live(() => {
      made += 1;
      return new Fake();
    });
    l.start();
    l.start();
    expect(made).toBe(1);
    l.stop();
  });
});
