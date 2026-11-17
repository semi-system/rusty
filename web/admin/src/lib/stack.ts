// On a narrow screen a table becomes a list of cards (app.css, `max-width: 40rem`); each cell then says which column it is in. The names come from the header cells, so a page
// only has to mark its table (`use:labelled`); rows that are added or replaced later get their labels too.
export function labelled(table: HTMLTableElement): { destroy: () => void } {
  const apply = (): void => {
    const heads = [...table.querySelectorAll('thead th')].map((th) => th.textContent?.trim() ?? '');
    for (const row of table.querySelectorAll('tbody tr')) {
      [...row.children].forEach((cell, i) => {
        const name = heads[i];
        if (cell.tagName === 'TD' && name) cell.setAttribute('data-label', name);
      });
    }
  };
  apply();
  // Only the structure is watched: setting an attribute does not trigger this again.
  const watch = new MutationObserver(apply);
  watch.observe(table, { childList: true, subtree: true });
  return { destroy: () => watch.disconnect() };
}
