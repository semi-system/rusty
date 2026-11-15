# The admin web front end (`web/admin`)

Svelte 5 (runes) and TypeScript, built with Vite. No SvelteKit, no router library, no runtime dependency beyond Svelte's compiled output
(design section 8: `docs/superpowers/plans/2026-11-14-admin-web-design.md`).

## What is there

| Path | What |
|---|---|
| `src/api/client.ts` | the API as typed calls: the JSON content type and the session's CSRF token on every mutation (the browser adds `Origin`), a failure is a **fixed code**, never text to show |
| `src/api/types.ts` | **generated** from `crates/rusty-admin/contract/v1.json` by `scripts/gen-types.mjs` (never edited by hand; a test fails when it is stale) |
| `src/i18n/en.ts`, `src/i18n/index.ts` | the message catalogue and `t(key, params)`; another language is another file that must satisfy the same type |
| `src/lib/router.ts` | a small client-side router (`matchRoute`, `href`) |
| `src/lib/app.svelte.ts` | the application state (runes): who is signed in, or which page is first |
| `src/pages/` | sign-in, first-administrator setup, the signed-in home (the other pages follow) |

## Rules that are enforced, not hoped for

* **No literal text in a template or in `aria-label`, `title`, `placeholder`, `alt`**: every string a person reads is a key of the catalogue (decision Q6). ESLint fails the build on a
  literal; a test renders the pages with a **pseudo-locale** (every message wrapped and lengthened) and fails on any visible string that did not come from the catalogue.
* **`{@html}` is forbidden** (lint). **`eval`, `new Function`, string timers, `document.write`, inline script or style, `javascript:` URLs, a third-party origin or a source map in the
  build output fail `scripts/check-bundle.mjs`.**
* The server sends **codes, not sentences** (`unauthorized`, `throttled`, ...); the client maps each to a catalogue key. An unknown or odd code becomes the generic message.
* A client route **never ends in a name that may contain a dot**: the server answers a path that looks like a file and does not exist with `404`, so `/domains/example.org` is not a page;
  `/domains/example.org/overview` is.

## Supply chain (the tooling is the risk of this change)

* `npm ci --ignore-scripts` from the committed lockfile (integrity hashes); `package.json` pins **exact** versions.
* `scripts/check-deps.mjs`: every package of the lockfile must be on `deps.allow` (a new package fails until the list is changed in a reviewed commit), come from the npm registry with a
  `sha512` hash, have **no install script** (`fsevents`, macOS-only and never installed on Linux, is the one named exception) and carry an allowed licence. MPL-2.0 appears in build tools
  only; the shipped bundle holds our compiled code and Svelte's runtime.
* `npm audit --omit=dev` must be clean (the shipped bundle has no runtime dependency); the tooling tree is reported and fails only on a **critical** advisory. Known at the time of writing, all in
  build tools with no path into the bundle: `deepmerge` (via `@sveltejs/vite-plugin-svelte`, no fix available) and `minimatch` (via `typescript-eslint`).
* The build is **reproducible**: `scripts/check-reproducible.sh` builds twice and compares the hash of every file.
* All of it runs in CI in the Node image pinned by digest in `deploy/images.lock` (`scripts/ci-web-docker.sh`, job `web`); locally: `bash scripts/ci-web.sh` with Node 22 on `PATH`.

## Building and embedding

```
cd web/admin && npm ci --ignore-scripts && npx vite build      # -> web/admin/dist
cargo build -p rusty-admin                                       # embeds dist (build.rs) when it exists
RUSTY_ADMIN_WEB_REQUIRED=1 cargo build -p rusty-admin --release  # a build error without a front end: for the release and image builds
RUSTY_ADMIN_WEB_DIST=/path/to/dist cargo build -p rusty-admin    # another directory
```

Without a `dist` directory the binary is still a working API server and logs that no front end is built in. With one, the service serves `/assets/<hash>.js|css` as `immutable`, the index page for
every non-API route (revalidated by its `ETag`) and the security headers of design section 6 on everything. Development: `npm run dev` proxies `/api` to `127.0.0.1:8080`.

## Pages

| Route | Page | Who |
|---|---|---|
| `/` | domains (list by cursor, create) | every administrator (a domain administrator sees its own domain, and not the creation form) |
| `/domains/{name}/overview` | status, enable/disable (platform administrators), the outgoing queue figures | |
| `/domains/{name}/accounts` | list, create (with the role addresses the first account receives), set a password, disable/enable | |
| `/domains/{name}/aliases` | list, create, remove | |
| `/domains/{name}/dkim` | the public records (selectable fields and a copy button), make a key; the private key is never shown | |
| `/domains/{name}/dns` | the records with the status of their last check, queue a check, reload | |
| `/domains/{name}/relay` | the domain's own relay (host, port, encryption, login, SPF include), test, remove; the password is never read back and is cleared from the page once sent | |
| `/domains/{name}/storage` | the domain's S3 store: status and last test for every administrator; settings, keys (never read back, cleared once sent) and the test button for a platform administrator | |

The component tests run each page against a fake server that records the call, its CSRF header and its body, and check what an administrator may do (a domain administrator is not offered
the platform actions; another tenant's domain is the same "not found" as a missing one), that a password never stays on the page, that an unknown value from the server (a status, a check
result) becomes the generic message rather than being shown, and the fixed message of each refusal.
