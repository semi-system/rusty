import { describe, expect, it } from 'vitest';
import { ApiError, Client, errorMessage, type Fetch } from './client';

type Call = { path: string; init: RequestInit };

/** A fake server: answers in order, remembers what it was asked. */
function fake(...answers: Array<{ status: number; body?: unknown; headers?: Record<string, string> } | 'down'>): { fetcher: Fetch; calls: Call[] } {
  const calls: Call[] = [];
  const fetcher: Fetch = async (path, init) => {
    calls.push({ path, init: init ?? {} });
    const a = answers.shift();
    if (a === undefined || a === 'down') {
      throw new TypeError('failed to fetch');
    }
    return new Response(a.body === undefined ? '' : JSON.stringify(a.body), { status: a.status, headers: a.headers });
  };
  return { fetcher, calls };
}

const ME = { address: 'root@example.org', scope: 'platform', domain: null, csrf: 'c'.repeat(64) };

describe('Client', () => {
  it('sends the CSRF token and the JSON content type on a mutation, and neither on a read', async () => {
    const { fetcher, calls } = fake({ status: 200, body: ME }, { status: 200, body: {} });
    const c = new Client(fetcher);
    expect(await c.me()).toEqual(ME);
    await c.signOut();
    const read = calls[0]!.init.headers as Record<string, string>;
    const write = calls[1]!.init.headers as Record<string, string>;
    expect(read['X-CSRF-Token']).toBeUndefined();
    expect(read['Content-Type']).toBeUndefined();
    expect(write['X-CSRF-Token']).toBe(ME.csrf);
    expect(write['Content-Type']).toBe('application/json');
    expect(calls[1]!.init.method).toBe('DELETE');
    expect(calls.every((x) => x.init.credentials === 'same-origin' && x.init.cache === 'no-store')).toBe(true);
  });

  it('remembers the token from sign-in and forgets it on sign-out', async () => {
    const { fetcher, calls } = fake({ status: 200, body: ME }, { status: 200, body: {} }, { status: 200, body: {} });
    const c = new Client(fetcher);
    expect(c.csrfToken).toBeNull();
    await c.signIn('root@example.org', 'pw');
    expect(c.csrfToken).toBe(ME.csrf);
    expect(JSON.parse(String(calls[0]!.init.body))).toEqual({ address: 'root@example.org', password: 'pw' });
    await c.signOut();
    expect(c.csrfToken).toBeNull();
  });

  it('treats 401 on the session as nobody and any other failure as an error', async () => {
    const c = new Client(fake({ status: 401, body: { error: 'unauthorized' } }).fetcher);
    expect(await c.me()).toBeNull();
    const d = new Client(fake({ status: 503, body: { error: 'busy' } }).fetcher);
    await expect(d.me()).rejects.toMatchObject({ status: 503, code: 'busy' });
  });

  it('reads the setup page as open, closed (404) or failing', async () => {
    expect(await new Client(fake({ status: 200, body: { required: true } }).fetcher).setupOpen()).toBe(true);
    expect(await new Client(fake({ status: 404, body: { error: 'not_found' } }).fetcher).setupOpen()).toBe(false);
    await expect(new Client(fake({ status: 503, body: { error: 'busy' } }).fetcher).setupOpen()).rejects.toBeInstanceOf(ApiError);
  });

  it('setup signs in through the session afterwards, which gives the CSRF token', async () => {
    const { fetcher, calls } = fake({ status: 201, body: ME }, { status: 200, body: ME });
    const c = new Client(fetcher);
    const me = await c.setup('t'.repeat(64), 'root@example.org', 'a-long-password-1');
    expect(me).toEqual(ME);
    expect(c.csrfToken).toBe(ME.csrf);
    expect(calls.map((x) => x.path)).toEqual(['/api/v1/setup', '/api/v1/session']);
  });

  it('turns a failure into the fixed code and keeps the delay of a throttled one', async () => {
    const c = new Client(fake({ status: 429, body: { error: 'throttled' }, headers: { 'retry-after': '90' } }).fetcher);
    const e = await c.signIn('a@b.c', 'x').catch((x: unknown) => x);
    expect(e).toBeInstanceOf(ApiError);
    expect(e).toMatchObject({ status: 429, code: 'throttled', retryAfter: 90 });
  });

  it('does not trust the body of an error: a missing or odd code is unknown', async () => {
    for (const body of [undefined, 'text', { error: 5 }, { message: 'x' }, null]) {
      const c = new Client(fake({ status: 500, body }).fetcher);
      const e = await c.signIn('a@b.c', 'x').catch((x: unknown) => x);
      expect(e).toMatchObject({ code: 'unknown' });
    }
  });

  it('a network failure is not an API error', async () => {
    const c = new Client(fake('down').fetcher);
    await expect(c.me()).rejects.not.toBeInstanceOf(ApiError);
  });
});

describe('errorMessage', () => {
  it('maps every code of the contract to a catalogue key and unknown ones to the generic message', () => {
    for (const code of ['unauthorized', 'invalid', 'forbidden', 'not_found', 'exists', 'unsupported', 'busy', 'unavailable']) {
      expect(errorMessage(new ApiError(400, code)).key).toBe(`error.${code}`);
    }
    expect(errorMessage(new ApiError(500, '<script>')).key).toBe('error.unknown');
    expect(errorMessage(new ApiError(429, 'throttled', 30))).toEqual({ key: 'error.throttled', params: { seconds: 30 } });
    expect(errorMessage(new ApiError(429, 'throttled')).key).toBe('error.throttledNow');
    expect(errorMessage(new TypeError('x')).key).toBe('error.network');
  });
});
