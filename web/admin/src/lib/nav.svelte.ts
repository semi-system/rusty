// The current path of the page, reactive. Links and the browser's back button change it; nothing reloads.

class Nav {
  path = $state(typeof location === 'undefined' ? '/' : location.pathname);

  constructor() {
    if (typeof window !== 'undefined') {
      window.addEventListener('popstate', () => {
        this.path = location.pathname;
      });
    }
  }

  /** Go to `path` (a URL path of this site). */
  go(path: string, replace = false): void {
    if (replace) {
      history.replaceState(null, '', path);
    } else if (path !== this.path) {
      history.pushState(null, '', path);
    }
    this.path = path;
  }
}

export const nav = new Nav();
