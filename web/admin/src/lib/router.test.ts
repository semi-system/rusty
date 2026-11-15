import { describe, expect, it } from 'vitest';
import { href, matchRoute } from './router';

const routes = [
  { pattern: '/', value: 'home' },
  { pattern: '/domains/{name}', value: 'domain' },
  { pattern: '/domains/{name}/dns', value: 'dns' },
];

describe('matchRoute', () => {
  it('matches literals and parameters and decodes a parameter once', () => {
    expect(matchRoute(routes, '/')).toEqual({ value: 'home', params: {} });
    expect(matchRoute(routes, '/domains/example.org')).toEqual({ value: 'domain', params: { name: 'example.org' } });
    expect(matchRoute(routes, '/domains/ex%40mple/dns')).toEqual({ value: 'dns', params: { name: 'ex@mple' } });
    expect(matchRoute(routes, '/domains/a%252Fb')?.params.name).toBe('a%2Fb');
  });

  it('does not match a different shape, and a bad escape matches nothing', () => {
    for (const p of ['/nothing', '/domains', '/domains/a/b/c', '/domains/a/other', '/domains/%E0%A4%A']) {
      expect(matchRoute(routes, p), p).toBeNull();
    }
  });
});

describe('href', () => {
  it('encodes the parameters it fills in', () => {
    expect(href('/domains/{name}/dns', { name: 'a b/c' })).toBe('/domains/a%20b%2Fc/dns');
    expect(href('/')).toBe('/');
  });
});
