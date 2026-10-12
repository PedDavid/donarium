// Small behaviours for the server-rendered UI. No inline handlers, so the
// page works under a strict Content-Security-Policy (script-src 'self').
(() => {
  const PALETTES = ['neutral', 'blue', 'green', 'orange', 'violet'];
  const root = document.documentElement;

  const setCookie = (name, value) => {
    document.cookie = `${name}=${encodeURIComponent(value)}; path=/; max-age=31536000; samesite=lax`;
  };

  // --- Command palette (⌘K) ---
  // An ARIA combobox: focus stays in the input, and aria-activedescendant
  // tells assistive technology which option the arrow keys selected.
  const palette = () => document.getElementById('command-palette');
  const input = () => palette()?.querySelector('input[role="combobox"]');
  const results = () => document.getElementById('command-results');
  const items = () => Array.from(results()?.querySelectorAll('[role="option"]') ?? []);

  const setActive = (next) => {
    items().forEach((el) => {
      el.classList.toggle('active', el === next);
      el.setAttribute('aria-selected', el === next ? 'true' : 'false');
    });
    const box = input();
    if (next) {
      box?.setAttribute('aria-activedescendant', next.id);
      next.scrollIntoView({ block: 'nearest' });
    } else {
      box?.removeAttribute('aria-activedescendant');
    }
  };

  // Keeps the combobox state in step with whatever results are shown.
  const syncResults = () => {
    const list = items();
    input()?.setAttribute('aria-expanded', list.length ? 'true' : 'false');
    setActive(list.find((el) => el.classList.contains('active')) ?? null);
  };

  const openPalette = () => {
    const dialog = palette();
    if (!dialog || dialog.open) return;
    // Drop the previous results right away: until the new ones arrive,
    // Enter must not follow a result for a query that is no longer shown.
    results()?.replaceChildren();
    syncResults();
    dialog.showModal();
    const box = input();
    box.value = '';
    box.focus();
    htmx.trigger(box, 'palette-open');
  };

  document.addEventListener('htmx:afterSwap', (event) => {
    if (event.target === results()) syncResults();
  });

  const move = (delta) => {
    const list = items();
    if (!list.length) return;
    const current = list.findIndex((el) => el.classList.contains('active'));
    setActive(list[(current + delta + list.length) % list.length]);
  };

  document.addEventListener('keydown', (event) => {
    const typing = event.target.closest('input, textarea, select, [contenteditable]');
    if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 'k') {
      event.preventDefault();
      openPalette();
    } else if (event.key === '/' && !typing) {
      event.preventDefault();
      openPalette();
    } else if (palette()?.open) {
      if (event.key === 'ArrowDown') { event.preventDefault(); move(1); }
      if (event.key === 'ArrowUp') { event.preventDefault(); move(-1); }
      if (event.key === 'Enter') {
        const active = items().find((el) => el.classList.contains('active'));
        if (active) { event.preventDefault(); active.click(); }
      }
    }
  });

  document.addEventListener('mousemove', (event) => {
    const item = event.target.closest('#command-palette [role="option"]');
    if (item && !item.classList.contains('active')) setActive(item);
  });

  document.addEventListener('click', (event) => {
    if (event.target.closest('[data-palette-open]')) {
      openPalette();
      return;
    }
    const opener = event.target.closest('[data-dialog-open]');
    if (opener) {
      document.getElementById(opener.dataset.dialogOpen)?.showModal();
      return;
    }
    const closer = event.target.closest('[data-dialog-close]');
    if (closer) {
      closer.closest('dialog')?.close();
      return;
    }
    // A click on the backdrop lands on the <dialog> element itself.
    if (event.target instanceof HTMLDialogElement) {
      event.target.close();
      return;
    }
    if (event.target.closest('[data-theme-toggle]')) {
      const dark = root.classList.toggle('dark');
      setCookie('donarium_theme', dark ? 'dark' : 'light');
      return;
    }
    if (event.target.closest('[data-palette-cycle]')) {
      const current = PALETTES.indexOf(root.dataset.palette || 'neutral');
      const next = PALETTES[(current + 1) % PALETTES.length];
      root.dataset.palette = next;
      setCookie('donarium_palette', next);
    }
  });


  // Closing a dialog clears the previous search results.
  document.addEventListener('close', (event) => {
    if (!(event.target instanceof HTMLDialogElement)) return;
    event.target.querySelector('#command-results')?.replaceChildren();
  }, true);

  // Follow the OS preference until the user picks a theme.
  if (!document.cookie.includes('donarium_theme=') && window.matchMedia('(prefers-color-scheme: dark)').matches) {
    root.classList.add('dark');
  }
})();
