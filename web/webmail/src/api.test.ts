import { describe, expect, it } from 'vitest';
import { addressesOf, ApiError, createApi } from './api';

interface Seen {
  path: string;
  init?: RequestInit;
}

function fake(replies: Array<{ status?: number; body: unknown }>): { doFetch: (p: string, i?: RequestInit) => Promise<Response>; seen: Seen[] } {
  const seen: Seen[] = [];
  const doFetch = async (path: string, init?: RequestInit): Promise<Response> => {
    seen.push({ path, init });
    const r = replies.shift() ?? { status: 500, body: {} };
    return new Response(JSON.stringify(r.body), { status: r.status ?? 200, headers: { 'content-type': 'application/json' } });
  };
  return { doFetch, seen };
}

describe('api', () => {
  it('remembers the token of the session and sends it, with JSON, on every change', async () => {
    const { doFetch, seen } = fake([{ body: { address: 'a@b.test', csrf: 'tok' } }, { body: { id: 7 } }, { body: {} }]);
    const api = createApi(doFetch);
    await api.signIn('a@b.test', 'pw');
    expect(seen[0]?.init?.headers).toMatchObject({ 'content-type': 'application/json', 'x-csrf-token': '' });
    await api.send({ to: ['x@y.test'], cc: [], bcc: [], subject: 's', text: 't' });
    expect(seen[1]?.init?.headers).toMatchObject({ 'x-csrf-token': 'tok', 'content-type': 'application/json' });
    await api.withdraw(7);
    expect(seen[2]?.init?.method).toBe('DELETE');
    expect(seen[2]?.init?.body).toBeUndefined();
  });

  it('reads with GET and no token, and a failure is the fixed code of the server', async () => {
    const { doFetch, seen } = fake([{ body: { items: [] } }, { status: 401, body: { error: 'unauthorized' } }]);
    const api = createApi(doFetch);
    expect(await api.folders()).toEqual([]);
    expect(seen[0]?.init?.headers).toEqual({});
    await expect(api.me()).rejects.toMatchObject({ status: 401, code: 'unauthorized' });
  });

  it('an unreachable server is a network error and not a crash', async () => {
    const api = createApi(() => Promise.reject(new TypeError('failed')));
    const e = await api.me().catch((x: unknown) => x);
    expect(e).toBeInstanceOf(ApiError);
    expect(e).toMatchObject({ status: 0, code: 'network' });
  });

  it('a page is asked for by cursor', async () => {
    const { doFetch, seen } = fake([{ body: { items: [], next: null } }]);
    await createApi(doFetch).messages(4, 120);
    expect(seen[0]?.path).toBe('/api/v1/folders/4/messages?limit=50&before=120');
  });

  it('the addresses of a field are split at commas, semicolons and spaces', () => {
    expect(addressesOf('a@x.test, b@y.test;c@z.test  d@w.test,')).toEqual(['a@x.test', 'b@y.test', 'c@z.test', 'd@w.test']);
    expect(addressesOf('  ')).toEqual([]);
  });
});
