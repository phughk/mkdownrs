// Mobile drawer: the sidebar slides in from the top bar. Desktop layout ignores all of this.
(() => {
  const root = document.documentElement;
  const toggle = document.querySelector("[data-nav-toggle]");
  const scrim = document.querySelector(".scrim");
  const sidebar = document.getElementById("sidebar");
  if (!toggle || !sidebar) return;
  const mobile = matchMedia("(max-width: 760px)");

  const set = (open) => {
    root.classList.toggle("nav-open", open);
    toggle.setAttribute("aria-expanded", String(open));
    toggle.setAttribute("aria-label", open ? "Close navigation" : "Open navigation");
    scrim.hidden = !open;
    // Keep the closed drawer out of the tab order and away from screen readers.
    sidebar.inert = mobile.matches && !open;
    if (open) sidebar.querySelector("a")?.focus({ preventScroll: true });
  };
  const sync = () => set(false);

  toggle.addEventListener("click", () => set(!root.classList.contains("nav-open")));
  scrim.addEventListener("click", () => { set(false); toggle.focus(); });
  sidebar.addEventListener("click", (e) => { if (e.target.closest("a")) set(false); });
  addEventListener("keydown", (e) => {
    if (e.key === "Escape" && root.classList.contains("nav-open")) { set(false); toggle.focus(); }
  });
  mobile.addEventListener("change", sync);
  sync();

  // Single-page export swaps the document title on navigation; mirror it in the top bar.
  const label = document.querySelector(".topbar-title");
  const titleEl = document.querySelector("title");
  if (label && titleEl) {
    new MutationObserver(() => { label.textContent = document.title; })
      .observe(titleEl, { childList: true, characterData: true, subtree: true });
  }
})();
