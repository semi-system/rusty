// A small client-side router (no framework router: design section 8). Paths are plain URLs; the server answers every non-API path with the index page.

export interface Route<T> {
  /** `/domains/{name}/dns`: literal segments and `{param}` segments. */
  pattern: string;
  value: T;
}

export interface Match<T> {
  value: T;
  params: Record<string, string>;
}

/** The first route whose pattern matches `path`; parameters are percent-decoded once. */
export function matchRoute<T>(routes: Route<T>[], path: string): Match<T> | null {
  const parts = split(path);
  for (const r of routes) {
    const pat = split(r.pattern);
    if (pat.length !== parts.length) continue;
    const params: Record<string, string> = {};
    let ok = true;
    for (let i = 0; i < pat.length; i++) {
      const p = pat[i] ?? '';
      const v = parts[i] ?? '';
      if (p.startsWith('{') && p.endsWith('}')) {
        try {
          params[p.slice(1, -1)] = decodeURIComponent(v);
        } catch {
          ok = false;
          break;
        }
      } else if (p !== v) {
        ok = false;
        break;
      }
    }
    if (ok) return { value: r.value, params };
  }
  return null;
}

function split(path: string): string[] {
  return path.split('/').filter((s) => s.length > 0);
}

/** The URL of a route with its parameters filled in and encoded. */
export function href(pattern: string, params: Record<string, string> = {}): string {
  const out = split(pattern).map((s) => (s.startsWith('{') && s.endsWith('}') ? encodeURIComponent(params[s.slice(1, -1)] ?? '') : s));
  return '/' + out.join('/');
}
