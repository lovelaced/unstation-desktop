// Ambient warm-glow field (replaces the kinetic constellation).
// A subordinate CSS background shown ONLY on non-video scenes via [data-ambient].
// No per-frame canvas; motion is pure CSS (frozen by the global reduced-motion rule
// and paused on blur via body.paused), so it never competes with the video or content.
//
// initAmbient(win) wires the blur/focus/visibility pause and returns a `setAmbient(on)`
// toggle the state machine calls per scene. Self-contained.
export function initAmbient(win) {
  const setAmbient = (on) => { win.dataset.ambient = on ? 'on' : 'off'; };
  // Pause the ambient field when the window loses focus / is hidden (it's purely
  // decorative — no reason to animate off-screen). Reduced-motion freezes it via CSS.
  addEventListener('blur', () => document.body.classList.add('paused'));
  addEventListener('focus', () => document.body.classList.remove('paused'));
  document.addEventListener('visibilitychange', () => document.body.classList.toggle('paused', document.hidden));
  setAmbient(false);
  return setAmbient;
}
