// A fake admin server for the component tests: answers by "METHOD path", can compute an answer from the request, and records every call with its headers and body.

import { Client, type Fetch } from './api/client';
import type { Me } from './api/types';

export type Answer = { status: number; body?: unknown; headers?: Record<string, string> };
export type Handler = Answer | Answer[] | ((body: unknown) => Answer);

export interface Call {
  key: string;
  body: unknown;
  headers: Record<string, string>;
}

export const PLATFORM: Me = { address: 'root@example.org', scope: 'platform', domain: null, csrf: 'c'.repeat(64) };
export const TENANT: Me = { address: 'boss@example.org', scope: 'domain', domain: 7, csrf: 'd'.repeat(64) };

export function fakeServer(routes: Record<string, Handler>): { client: Client; calls: Call[]; last: (key: string) => Call | undefined } {
  const calls: Call[] = [];
  const fetcher: Fetch = async (path, init) => {
    const key = `${init?.method ?? 'GET'} ${path}`;
    // A piece of an upload is bytes, not JSON: it is recorded as it is.
    const body = init?.body === undefined ? undefined : typeof init.body === 'string' ? JSON.parse(init.body) : init.body;
    calls.push({ key, body, headers: (init?.headers ?? {}) as Record<string, string> });
    const h = routes[key];
    const a = typeof h === 'function' ? h(body) : Array.isArray(h) ? h.shift() : h;
    if (a === undefined) {
      return new Response(JSON.stringify({ error: 'not_found' }), { status: 404 });
    }
    return new Response(a.body === undefined ? '' : JSON.stringify(a.body), { status: a.status, headers: a.headers });
  };
  return { client: new Client(fetcher), calls, last: (key) => [...calls].reverse().find((c) => c.key === key) };
}

/** The answers every signed-in page needs. */
export function signedIn(me: Me, extra: Record<string, Handler> = {}): Record<string, Handler> {
  return { 'GET /api/v1/session': { status: 200, body: me }, ...extra };
}

export const ok = (body: unknown = {}): Answer => ({ status: 200, body });
export const created = (body: unknown): Answer => ({ status: 201, body });
export const fail = (status: number, error: string, headers?: Record<string, string>): Answer => ({ status, body: { error }, headers });
