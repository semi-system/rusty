# Design: the look of the admin console

Status: built 2026-11-17 (the owner asked for a calmer, more professional console). This page records the choices so the next pages keep to them. The structure of the pages and the API are unchanged
(`design-admin-web.md`); this is the visual and layout layer: one stylesheet (`web/admin/src/app.css`), a few shared components (`web/admin/src/lib`), the frame (`web/admin/src/pages/Shell.svelte`).

## Who uses it and what they do

An administrator who keeps mail flowing: they scan for what is wrong, fix it, and copy technical values (DNS records, relay and bucket settings) to other systems. So the console is built to be **scanned
and trusted**: status in words with colour, technical values in a fixed-width face, one primary action per place, no decoration.

## Choices

* **Palette.** A cool grey canvas, white surfaces, a graphite sidebar, and one accent: patina green-blue (`#0f6b63`, `#3fb5aa` in the dark theme). Green, amber and red are for status only and always come with a
  word (never colour alone). The dark theme follows the browser's setting (`prefers-color-scheme`); every colour is a token in `:root`.
* **Type.** The system's own interface font (Segoe UI Variable, then the platform default), 14 px, tabular figures; headings 600 weight with a slightly negative tracking. Fixed-width type is only for **values**
  (addresses of records, versions, instance names), never for small labels. Sentence case everywhere; no all-caps labels.
* **Layout.** A fixed sidebar (Manage, Monitoring) with the product name; a top bar with a breadcrumb (where you are) and who you are; the page: a head (title, one line that says what the page is for, the main actions),
  then sections. A domain has tabs under its name. Below 60 rem the sidebar is a drawer opened by the menu button (closed by a choice, the scrim or Escape).
* **Surfaces.** Tables sit on a white surface with hairline rows, a quiet header and a hover row: no zebra, no card around every block. Panels (a bordered white surface) are for forms and for a group of status
  lines; stat tiles are for counts. Two radii only (6 px controls, 10 px surfaces); no shadows except the open drawer.
* **Status.** A badge is a word with a dot (`Active`, `Missing`, `Degraded`); an alert is a left-bordered strip (`ok`, `warn`, `danger`). The domain overview shows **setup progress**: how many of the required DNS
  records the last check found correct, and the outgoing queue as four figures that turn amber or red when something is failing.
* **Controls.** One primary button per group (accent); every other action is a quiet outlined button; a destructive action is outlined in red. Row actions say a short word (`Disable`); the accessible name
  keeps the full sentence with the address. A field has its label above it and its hint under it.
* **Motion.** Only what answers an action: colour changes on hover and focus, the drawer sliding. Nothing animates on load. `prefers-reduced-motion` is respected.
* **Accessibility.** A visible focus ring on everything; the contrast of text and status colours is at least 4.5:1 in both themes; headings are real headings (one h1 per page); tables have header cells and names;
  the skip link goes to the content; the drawer is a button with `aria-expanded`.

## Rules for new pages

1. Start with `PageHead` (or `DomainShell` inside a domain). 2. A list is a `table-wrap` table, or an `empty` block that says what to do. 3. A form is a `panel` with a `form-grid`; the main button is last.
4. A status is a `Badge`; a result of an action is an `alert`. 5. Text goes in the catalogue (`i18n/en.ts`); a new page adds its one-line `lede`. 6. No inline styles (the content security policy forbids them): a class,
or a step class for a computed width (`progress-fill p40`).

## Looking at it

`web/admin/scripts/preview.mjs` serves the built console with a fake API and a believable installation (`npm run build && node scripts/preview.mjs --port 4173`; `--scope domain` for a domain administrator,
`--state signin` for the sign-in page). It is a design aid, not part of the product or of the build; it needs no database.
