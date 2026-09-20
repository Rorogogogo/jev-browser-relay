// Short adaptive wait after input. Never a long fixed sleep.
//
// Waits for two animation frames, capped at 50 ms. When the field is a combobox, keeps waiting
// until its own listbox shows a visible option, capped at 200 ms — that is the one case where
// acting immediately reliably races the page.
//
// Adapted from browser-use/jev-ultrafast (MIT).
(action => new Promise(resolve => {
  const field = window.__jevRelay?.nodes.get(action.node);
  const autocomplete = action.kind === 'fill' && field?.getAttribute('role') === 'combobox';
  let frames = 0, stopped = false;
  const finish = () => { stopped = true; resolve(); };
  setTimeout(finish, autocomplete ? 200 : 50);
  const ready = () => {
    if (stopped) return;
    const ids = (field?.getAttribute('aria-controls') || field?.getAttribute('aria-owns') || '')
      .split(/\s+/).filter(Boolean);
    const roots = ids.length ? ids.map(id => document.getElementById(id)).filter(Boolean) : [document];
    const options = roots.flatMap(root => [...root.querySelectorAll('[role="option"]')]);
    if (++frames >= 2 && (!autocomplete || options.some(o => {
      const r = o.getBoundingClientRect();
      return r.width && r.height && r.bottom > 0 && r.top < innerHeight &&
        o.checkVisibility({checkOpacity: true, checkVisibilityCSS: true});
    }))) finish();
    else requestAnimationFrame(ready);
  };
  requestAnimationFrame(ready);
}))
