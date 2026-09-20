// Resolve an observed node to live geometry, re-verify it, and apply a <select> value.
//
// Returns null whenever the target is no longer safe to touch: detached, hidden, disabled,
// read-only, off-viewport, or covered by another element. The caller treats null as a stale
// page and re-observes rather than clicking blind.
//
// Adapted from browser-use/jev-ultrafast (MIT).
(action => {
  const cache = window.__jevRelay;
  const e = cache?.nodes.get(action.node);
  if (!e?.isConnected || e.matches(':disabled') || e.closest('[aria-disabled="true"],[inert]') ||
      !e.checkVisibility({checkOpacity: true, checkVisibilityCSS: true})) return null;
  if (action.kind === 'fill' && (e.readOnly || e.getAttribute('aria-readonly') === 'true')) return null;

  const r = e.getBoundingClientRect(), x = r.x + r.width / 2, y = r.y + r.height / 2;
  if (!r.width || !r.height || x < 0 || y < 0 || x >= innerWidth || y >= innerHeight) return null;
  // Hit-test: if the centre point belongs to something else, an overlay is in the way.
  if (!e.contains(document.elementFromPoint(x, y))) return null;

  if (action.kind === 'select') {
    if (e.tagName !== 'SELECT' || ![...e.options].some(o =>
        o.value === action.value && !o.disabled && !o.closest('optgroup[disabled]'))) return null;
    e.value = action.value;
    e.dispatchEvent(new Event('input', {bubbles: true}));
    e.dispatchEvent(new Event('change', {bubbles: true}));
  }
  return {x, y};
})
