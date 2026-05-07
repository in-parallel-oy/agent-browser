// Synthetic animated cursor overlay for screen recordings.
//
// Installed via Page.addScriptToEvaluateOnNewDocument (with runImmediately:true)
// when `record start` runs with a cursor flag set. The script is a no-op on
// non-top frames and is idempotent across same-context navigations.
//
// Hardening:
//   - Top-layer Popover host so a page's own <dialog>.showModal() can't paint
//     over us (popover and dialog both stack in the top layer in opening
//     order; we re-show on MutationObserver tick if needed). Falls back to a
//     fixed-position element with max z-index when Popover isn't available.
//   - Closed Shadow DOM mounted on documentElement (not body), so page-side
//     document.body.querySelectorAll walks don't see us.
//   - Constructable CSSStyleSheet via adoptedStyleSheets, plus per-property
//     element.style.x = '...' assignments only. Never inline-stylesheet
//     elements, never style-attribute assignment, never element.style direct
//     text assignment. These are the only paths that bypass strict
//     `style-src` CSP.
//   - aria-hidden + role=presentation belt-and-suspenders for AX trees.
//   - Symbol.for('__ab_cursor__') guard reduces collision likelihood with
//     content scripts that happen to use the same window key.
//   - prefers-reduced-motion auto-zeros the tween unless cfg.motion === 'always'.

(() => {
  const GUARD = Symbol.for('__ab_cursor__');
  if (window[GUARD]) return;
  if (window.self !== window.top) return;
  window[GUARD] = true;

  const HOST_ID = '__ab_cursor_root__';
  const cfg = window.__ab_cursor_config || {
    size: 28,
    tweenMs: 250,
    clickMs: 150,
    motion: 'auto',
    svg: '',
  };

  const reducedMotion =
    typeof window.matchMedia === 'function' &&
    window.matchMedia('(prefers-reduced-motion: reduce)').matches;
  const motionDefault =
    cfg.motion === 'off'
      ? 0
      : cfg.motion === 'always' || !reducedMotion
        ? cfg.tweenMs
        : 0;

  const host = document.createElement('div');
  host.id = HOST_ID;
  host.setAttribute('aria-hidden', 'true');
  host.setAttribute('role', 'presentation');
  host.style.position = 'fixed';
  host.style.top = '0';
  host.style.left = '0';
  host.style.right = '0';
  host.style.bottom = '0';
  host.style.width = '100%';
  host.style.height = '100%';
  host.style.margin = '0';
  host.style.padding = '0';
  host.style.border = '0';
  host.style.background = 'transparent';
  host.style.pointerEvents = 'none';
  host.style.zIndex = '2147483647';
  host.style.overflow = 'visible';

  const supportsPopover =
    typeof HTMLElement !== 'undefined' &&
    'popover' in HTMLElement.prototype &&
    typeof host.showPopover === 'function';
  if (supportsPopover) {
    try {
      host.popover = 'manual';
    } catch (_) {
      // older interpretations of the API; fall back to plain fixed positioning.
    }
  }

  const shadow = host.attachShadow({ mode: 'closed' });

  // Constructable stylesheet -- not subject to style-src CSP.
  const sheet = new CSSStyleSheet();
  sheet.replaceSync(
    [
      ':host { all: initial; }',
      '.cursor {',
      '  position: absolute;',
      '  top: 0;',
      '  left: 0;',
      '  width: ' + cfg.size + 'px;',
      '  height: ' + cfg.size + 'px;',
      '  pointer-events: none;',
      '  will-change: transform;',
      '  transform: translate(-1000px, -1000px);',
      '  color: #111;',
      '  filter: drop-shadow(0 1px 2px rgba(0,0,0,0.35));',
      '}',
      '.ripple {',
      '  position: absolute;',
      '  top: 0;',
      '  left: 0;',
      '  width: ' + cfg.size + 'px;',
      '  height: ' + cfg.size + 'px;',
      '  pointer-events: none;',
      '  border-radius: 50%;',
      '  background: rgba(255, 255, 255, 0.65);',
      '  border: 2px solid rgba(0, 0, 0, 0.45);',
      '  opacity: 0;',
      '  will-change: transform, opacity;',
      '  transform: translate(-1000px, -1000px) scale(0);',
      '}',
    ].join('\n'),
  );
  shadow.adoptedStyleSheets = [sheet];

  const cursorEl = document.createElement('div');
  cursorEl.className = 'cursor';
  // SVG is supplied as a string from the config; assign via innerHTML so that
  // the SVG namespaces are parsed correctly. innerHTML is not gated by CSP
  // when the source is page-script-controlled (no Trusted Types unless the
  // page enables them, in which case this overlay is one of the things a
  // page may legitimately reject -- documented limitation).
  cursorEl.innerHTML = cfg.svg || '';

  const rippleEl = document.createElement('div');
  rippleEl.className = 'ripple';

  shadow.appendChild(cursorEl);
  shadow.appendChild(rippleEl);

  document.documentElement.appendChild(host);

  if (supportsPopover) {
    try {
      host.showPopover();
    } catch (_) {
      // Popover may fail if the host is somehow detached; ignore and rely
      // on z-index.
    }
    // Re-promote into the top layer if a page modal/popover preempts us.
    // Cheap to observe document-level childList; we don't need subtree.
    try {
      const mo = new MutationObserver(() => {
        if (host.isConnected && typeof host.showPopover === 'function') {
          if (!host.matches(':popover-open')) {
            try {
              host.showPopover();
            } catch (_) {}
          }
        }
      });
      mo.observe(document.documentElement, { childList: true, subtree: false });
    } catch (_) {
      // MutationObserver should always exist; ignore if the page polluted it.
    }
  }

  const state = { x: -1000, y: -1000, animation: null };

  function applyImmediate(x, y) {
    state.x = x;
    state.y = y;
    cursorEl.style.transform = 'translate(' + x + 'px, ' + y + 'px)';
  }

  function moveTo(x, y, ms) {
    const duration = Math.max(
      0,
      typeof ms === 'number' ? ms : motionDefault,
    );
    const fromX = state.x;
    const fromY = state.y;
    if (duration === 0 || fromX < 0 || fromY < 0) {
      applyImmediate(x, y);
      return Promise.resolve();
    }
    if (state.animation) {
      try {
        state.animation.cancel();
      } catch (_) {}
    }
    state.x = x;
    state.y = y;
    let anim;
    try {
      anim = cursorEl.animate(
        [
          { transform: 'translate(' + fromX + 'px, ' + fromY + 'px)' },
          { transform: 'translate(' + x + 'px, ' + y + 'px)' },
        ],
        {
          duration: duration,
          easing: 'cubic-bezier(0.22, 1, 0.36, 1)',
          fill: 'forwards',
        },
      );
    } catch (_) {
      applyImmediate(x, y);
      return Promise.resolve();
    }
    state.animation = anim;
    return anim.finished.then(
      () => undefined,
      () => undefined,
    );
  }

  function click(x, y) {
    try {
      rippleEl.animate(
        [
          {
            transform: 'translate(' + x + 'px, ' + y + 'px) scale(0)',
            opacity: 0.65,
          },
          {
            transform: 'translate(' + x + 'px, ' + y + 'px) scale(2.2)',
            opacity: 0,
          },
        ],
        {
          duration: cfg.clickMs,
          easing: 'ease-out',
          fill: 'forwards',
        },
      );
    } catch (_) {
      // WAAPI not available; ignore (cursor still tweens via fallback).
    }
  }

  function destroy() {
    try {
      if (state.animation) state.animation.cancel();
    } catch (_) {}
    try {
      host.remove();
    } catch (_) {}
    try {
      delete window[GUARD];
      delete window.__ab_cursor;
    } catch (_) {}
  }

  window.__ab_cursor = { moveTo: moveTo, click: click, destroy: destroy, state: state };
})();
