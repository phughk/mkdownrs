const dark = window.matchMedia("(prefers-color-scheme: dark)").matches;
mermaid.initialize({ startOnLoad: false, theme: dark ? "dark" : "default" });

// Renders not-yet-processed diagrams under `root`. Hidden elements can't be
// measured, so callers should only pass visible roots.
window.renderMermaid = (root) =>
  mermaid.run({ nodes: root.querySelectorAll("pre.mermaid:not([data-processed])") });

function expand(svg) {
  const overlay = document.createElement("div");
  overlay.className = "mermaid-overlay";
  const stage = document.createElement("div");
  stage.className = "stage";
  const clone = svg.cloneNode(true);
  const vb = svg.viewBox.baseVal;
  const rect = svg.getBoundingClientRect();
  const w = vb && vb.width ? vb.width : rect.width;
  const h = vb && vb.height ? vb.height : rect.height;
  clone.removeAttribute("style");
  clone.setAttribute("width", w);
  clone.setAttribute("height", h);
  stage.appendChild(clone);
  overlay.appendChild(stage);
  const hint = document.createElement("div");
  hint.className = "hint";
  hint.textContent = "Drag to pan · pinch or scroll to zoom · Esc to close";
  overlay.appendChild(hint);
  document.body.appendChild(overlay);

  let scale = Math.min(innerWidth / w, innerHeight / h, 1) * 0.9;
  let x = (innerWidth - w * scale) / 2;
  let y = (innerHeight - h * scale) / 2;
  const apply = () => { stage.style.transform = `translate(${x}px, ${y}px) scale(${scale})`; };
  apply();

  const zoomAt = (cx, cy, factor) => {
    const next = Math.min(Math.max(scale * factor, 0.05), 20);
    x = cx - (cx - x) * (next / scale);
    y = cy - (cy - y) * (next / scale);
    scale = next;
    apply();
  };
  overlay.addEventListener("wheel", (e) => {
    e.preventDefault();
    zoomAt(e.clientX, e.clientY, Math.exp(-e.deltaY * (e.ctrlKey ? 0.01 : 0.002)));
  }, { passive: false });

  // Track every active pointer: one pans, two pan + pinch-zoom around their midpoint.
  const pointers = new Map();
  let moved = false;
  let gesture = null;
  const measure = () => {
    const [a, b] = [...pointers.values()];
    if (!b) return { cx: a.x, cy: a.y, dist: 0 };
    return { cx: (a.x + b.x) / 2, cy: (a.y + b.y) / 2, dist: Math.hypot(a.x - b.x, a.y - b.y) };
  };
  overlay.addEventListener("pointerdown", (e) => {
    if (pointers.size === 0) moved = false;
    pointers.set(e.pointerId, { x: e.clientX, y: e.clientY, sx: e.clientX, sy: e.clientY });
    overlay.classList.add("dragging");
    gesture = measure();
    try { overlay.setPointerCapture(e.pointerId); } catch {}
  });
  overlay.addEventListener("pointermove", (e) => {
    const p = pointers.get(e.pointerId);
    if (!p) return;
    p.x = e.clientX;
    p.y = e.clientY;
    if (Math.abs(p.x - p.sx) + Math.abs(p.y - p.sy) > 4) moved = true;
    const now = measure();
    x += now.cx - gesture.cx;
    y += now.cy - gesture.cy;
    if (pointers.size > 1 && gesture.dist > 0) zoomAt(now.cx, now.cy, now.dist / gesture.dist);
    else apply();
    gesture = now;
  });
  const release = (e) => {
    if (!pointers.delete(e.pointerId)) return;
    // A tap on the background (no drag, no pinch) closes the overlay.
    if (pointers.size === 0) {
      overlay.classList.remove("dragging");
      if (!moved && e.type === "pointerup" && !stage.contains(e.target)) close();
    } else {
      gesture = measure();
      moved = true;
    }
  };
  overlay.addEventListener("pointerup", release);
  overlay.addEventListener("pointercancel", release);
  const onKey = (e) => { if (e.key === "Escape") close(); };
  function close() {
    overlay.remove();
    document.removeEventListener("keydown", onKey);
  }
  document.addEventListener("keydown", onKey);
}

// Diagram/Source switch: flip which of the two <pre>s is shown.
document.addEventListener("click", (e) => {
  const button = e.target.closest(".mermaid-switch button");
  if (!button) return;
  const block = button.closest(".mermaid-block");
  const view = button.dataset.view;
  block.dataset.view = view;
  block.querySelector("pre.mermaid").hidden = view !== "diagram";
  block.querySelector("pre.mermaid-source").hidden = view !== "source";
  for (const b of block.querySelectorAll(".mermaid-switch button")) {
    b.setAttribute("aria-selected", b === button);
  }
});

document.addEventListener("click", (e) => {
  const pre = e.target.closest("pre.mermaid");
  const svg = pre && pre.querySelector("svg");
  if (svg) expand(svg);
});
