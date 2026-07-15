/* unstation docs chrome — header condense, reading progress, gliding nav
   highlight, mobile menu, on-this-page scrollspy, code copy buttons.
   Vanilla and dependency-free; the page is fully readable without any of it,
   and prefers-reduced-motion flattens every transition in the CSS. */
(() => {
  "use strict";

  const head = document.querySelector(".site-head");
  const bar = document.querySelector(".progress-bar");
  const playhead = document.querySelector(".progress-head");
  const nav = document.querySelector(".site-nav");
  const pill = document.querySelector(".nav-pill");
  const menuBtn = document.querySelector(".menu-btn");
  const menu = document.querySelector(".site-menu");
  const scrim = document.querySelector(".menu-scrim");
  const doc = document.querySelector(".doc");
  const toc = document.querySelector(".toc");
  const foot = document.querySelector(".site-foot");

  /* ---- scroll: condensed header, reading progress, scrollspy ---- */

  let headings = [];
  let tocLinks = new Map();
  const tocMarker = toc && toc.querySelector(".toc-marker");
  if (toc && doc) {
    for (const a of toc.querySelectorAll("a[href^='#']")) {
      const h = doc.querySelector(`[id="${decodeURIComponent(a.hash.slice(1))}"]`);
      if (h) { headings.push(h); tocLinks.set(h, a); }
    }
  }

  let current = null;
  const spy = () => {
    if (!headings.length) return;
    let next = null;
    for (const h of headings) {
      if (h.getBoundingClientRect().top <= 120) next = h;
      else break;
    }
    if (next === current) return;
    current = next;
    for (const [h, a] of tocLinks) a.classList.toggle("active", h === current);
    if (!tocMarker) return;
    const a = current && tocLinks.get(current);
    if (a) {
      tocMarker.style.opacity = "1";
      tocMarker.style.transform = `translateY(${a.offsetTop}px)`;
      tocMarker.style.height = `${a.offsetHeight}px`;
    } else {
      tocMarker.style.opacity = "0";
    }
  };

  let ticking = false;
  const onScroll = () => {
    if (ticking) return;
    ticking = true;
    requestAnimationFrame(() => {
      ticking = false;
      const y = scrollY;
      if (head) head.classList.toggle("scrolled", y > 16);
      const max = document.documentElement.scrollHeight - innerHeight;
      const p = max > 240 ? Math.min(1, Math.max(0, y / max)) : 0;
      if (head) head.classList.toggle("progressing", p > 0.004);
      if (bar) bar.style.transform = `scaleX(${p})`;
      if (playhead) playhead.style.transform = `translateX(${Math.max(0, p * innerWidth - 4)}px)`;
      spy();
    });
  };
  addEventListener("scroll", onScroll, { passive: true });

  /* ---- gliding hover highlight behind the nav links ---- */

  // The pill appears under the first link you point at, then glides between
  // links while you roam and fades away when you leave. The first placement is
  // instant (the .ready class enables the transition a frame later) so it
  // never flies in from the corner.
  const movePill = (a) => {
    if (!pill || !a) return;
    pill.style.left = `${a.offsetLeft}px`;
    pill.style.width = `${a.offsetWidth}px`;
    pill.style.opacity = "1";
    if (!pill.classList.contains("ready"))
      requestAnimationFrame(() => pill.classList.add("ready"));
  };
  const restPill = () => { if (pill) pill.style.opacity = "0"; };
  if (nav && pill) {
    nav.addEventListener("pointerover", (e) => {
      const a = e.target.closest("a");
      if (a) movePill(a);
    });
    nav.addEventListener("pointerleave", restPill);
    nav.addEventListener("focusin", (e) => {
      const a = e.target.closest("a");
      if (a) movePill(a);
    });
    nav.addEventListener("focusout", restPill);
  }

  /* ---- mobile menu ---- */

  const setMenu = (open) => {
    if (!menu || !menuBtn) return;
    document.documentElement.classList.toggle("menu-open", open);
    menuBtn.setAttribute("aria-expanded", String(open));
    menu.toggleAttribute("inert", !open);
    for (const el of [doc, toc, foot]) if (el) el.toggleAttribute("inert", open);
    if (!open && menu.contains(document.activeElement)) menuBtn.focus();
  };
  const menuIsOpen = () => document.documentElement.classList.contains("menu-open");

  if (menuBtn && menu) {
    menuBtn.addEventListener("click", () => setMenu(!menuIsOpen()));
    if (scrim) scrim.addEventListener("click", () => setMenu(false));
    addEventListener("keydown", (e) => {
      if (e.key === "Escape" && menuIsOpen()) setMenu(false);
    });
    // Same-page anchors still close the sheet; other links navigate away anyway.
    menu.addEventListener("click", (e) => {
      if (e.target.closest("a")) setMenu(false);
    });
    // Leaving the narrow layout while the sheet is open would strand it.
    matchMedia("(min-width: 1020px)").addEventListener("change", (e) => {
      if (e.matches && menuIsOpen()) setMenu(false);
    });
  }

  /* ---- copy buttons on code blocks ---- */

  if (navigator.clipboard && doc) {
    for (const pre of doc.querySelectorAll("pre")) {
      const btn = document.createElement("button");
      btn.type = "button";
      btn.className = "copy";
      btn.textContent = "Copy";
      btn.addEventListener("click", async () => {
        const code = pre.querySelector("code");
        try {
          await navigator.clipboard.writeText((code || pre).innerText.replace(/\n$/, ""));
          btn.textContent = "Copied";
          btn.classList.add("did");
          setTimeout(() => { btn.textContent = "Copy"; btn.classList.remove("did"); }, 1600);
        } catch { /* clipboard unavailable (e.g. non-secure context): leave the button inert */ }
      });
      pre.appendChild(btn);
    }
  }

  addEventListener("resize", () => { restPill(); onScroll(); }, { passive: true });
  onScroll();
})();
