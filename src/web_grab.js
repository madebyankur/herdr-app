// SPIKE: design mode's in-page picker (web.rs injects it with web.design).
// Hover highlights the element under the pointer; a click captures it and posts
// { grab: {...} } to the app; Escape posts { grab_cancel: true }. One shot:
// the overlay removes itself before posting, so the app's snapshot is clean.
(() => {
  if (window.__herdrGrab) return;
  const MAX_TEXT = 300, MAX_HTML = 3000, MAX_ANCESTORS = 6;
  const STYLE_KEYS = ["display", "position", "width", "height", "margin", "padding", "gap",
    "color", "background-color", "background-image", "border", "border-radius", "box-shadow",
    "font-family", "font-size", "font-weight", "line-height", "letter-spacing", "text-align",
    "opacity", "z-index", "flex-direction", "align-items", "justify-content"];
  // Attributes worth showing; the rest (data-*, tokens, event handlers) stay out.
  const SAFE_ATTRS = new Set(["id", "class", "role", "type", "name", "href", "src", "alt", "title",
    "aria-label", "aria-labelledby", "aria-describedby", "placeholder", "for", "target", "rel", "disabled"]);

  const clamp = (s, n) => (s = String(s ?? "")).length > n ? s.slice(0, n) + "…" : s;
  const text = (el, n) => clamp((el.innerText || el.textContent || "").replace(/\s+/g, " ").trim(), n);

  const host = document.createElement("div");
  host.style.cssText = "position:fixed;inset:0;z-index:2147483647;cursor:crosshair;";
  const root = host.attachShadow({ mode: "closed" });
  const layer = document.createElement("div");
  // The top layer draws above modals and popovers too.
  layer.popover = "manual";
  layer.style.cssText = "position:fixed;inset:0;margin:0;padding:0;border:0;width:100vw;height:100vh;max-width:none;max-height:none;background:transparent;overflow:visible;pointer-events:none;";
  const box = document.createElement("div");
  box.style.cssText = "position:fixed;display:none;pointer-events:none;border:2px solid #4c8dff;border-radius:3px;background:rgba(76,141,255,.10);box-shadow:0 0 0 1px rgba(255,255,255,.7);";
  const label = document.createElement("div");
  label.style.cssText = "position:fixed;display:none;pointer-events:none;padding:3px 8px;border-radius:5px;background:rgba(20,20,24,.92);color:#eee;font:11px/1.4 -apple-system,system-ui,sans-serif;white-space:nowrap;max-width:320px;overflow:hidden;text-overflow:ellipsis;";
  layer.append(box, label);
  root.append(layer);
  document.documentElement.append(host);
  try { layer.showPopover(); } catch {}

  let current = null;

  // The page element under a point: the overlay steps aside, open shadow roots are entered.
  function hit(x, y) {
    host.style.pointerEvents = "none";
    let el = document.elementFromPoint(x, y);
    while (el && el.shadowRoot) {
      const inner = el.shadowRoot.elementFromPoint(x, y);
      if (!inner || inner === el) break;
      el = inner;
    }
    host.style.pointerEvents = "";
    return el === document.documentElement || el === document.body ? null : el;
  }

  function show(el) {
    current = el;
    if (!el) { box.style.display = label.style.display = "none"; return; }
    const r = el.getBoundingClientRect();
    Object.assign(box.style, { display: "block", left: r.x + "px", top: r.y + "px", width: r.width + "px", height: r.height + "px" });
    const t = text(el, 30);
    label.textContent = [el.tagName.toLowerCase(), el.getAttribute("role") && "role=" + el.getAttribute("role"), t && `"${t}"`, `${Math.round(r.width)}×${Math.round(r.height)}`].filter(Boolean).join("  ");
    const below = r.bottom + 6 + 24 < innerHeight;
    Object.assign(label.style, { display: "block", left: Math.max(4, r.x) + "px", top: (below ? r.bottom + 6 : Math.max(4, r.top - 28)) + "px" });
  }

  // A selector that finds only this element (ids, then a short nth-of-type path).
  function selector(el) {
    const unique = (s) => { try { return document.querySelectorAll(s).length === 1; } catch { return false; } };
    if (el.id && unique("#" + CSS.escape(el.id))) return "#" + CSS.escape(el.id);
    const parts = [];
    for (let e = el; e && e.nodeType === 1 && e !== document.documentElement && parts.length < 8; e = e.parentElement) {
      let part = e.tagName.toLowerCase();
      if (e.id) { parts.unshift("#" + CSS.escape(e.id)); break; }
      const sibs = e.parentElement ? [...e.parentElement.children].filter((s) => s.tagName === e.tagName) : [];
      if (sibs.length > 1) part += `:nth-of-type(${sibs.indexOf(e) + 1})`;
      parts.unshift(part);
      if (unique(parts.join(" > "))) break;
    }
    return parts.join(" > ");
  }

  // The element without secrets: safe attributes only, form values never.
  function safeHtml(el) {
    const copy = el.cloneNode(true);
    const all = [copy, ...copy.querySelectorAll("*")];
    for (const n of all) {
      for (const a of [...n.attributes]) if (!SAFE_ATTRS.has(a.name)) n.removeAttribute(a.name);
      if (n.tagName === "INPUT" || n.tagName === "TEXTAREA") n.removeAttribute("value");
      if (n.tagName === "SCRIPT" || n.tagName === "STYLE") n.textContent = "";
    }
    return clamp(copy.outerHTML, MAX_HTML);
  }

  // React dev builds (≤18) record where a component was written.
  function source(el) {
    try {
      const key = Object.keys(el).find((k) => k.startsWith("__reactFiber$"));
      for (let f = key && el[key]; f; f = f.return) {
        const s = f._debugSource;
        if (s && s.fileName) {
          const name = typeof f.type === "function" ? f.type.displayName || f.type.name : null;
          return { file: s.fileName, line: s.lineNumber, component: name || null };
        }
      }
    } catch {}
    return null;
  }

  function capture(el) {
    const r = el.getBoundingClientRect();
    const cs = getComputedStyle(el);
    const styles = {};
    for (const k of STYLE_KEYS) { const v = cs.getPropertyValue(k); if (v && v !== "none" && v !== "normal" && v !== "auto") styles[k] = clamp(v, 200); }
    const ancestors = [];
    for (let p = el.parentElement; p && p !== document.body && ancestors.length < MAX_ANCESTORS; p = p.parentElement) {
      const cls = typeof p.className === "string" && p.className.trim() ? "." + p.className.trim().split(/\s+/).slice(0, 3).join(".") : "";
      ancestors.push(p.tagName.toLowerCase() + (p.id ? "#" + p.id : "") + cls);
    }
    return {
      page: { url: location.href, title: document.title, viewport: [innerWidth, innerHeight], dpr: devicePixelRatio },
      tag: el.tagName.toLowerCase(),
      role: el.getAttribute("role"),
      text: text(el, MAX_TEXT),
      selector: clamp(selector(el), 400),
      html: safeHtml(el),
      styles,
      ancestors,
      rect: { x: r.x, y: r.y, width: r.width, height: r.height },
      source: source(el),
    };
  }

  function stop() {
    removeEventListener("pointermove", onMove, true);
    removeEventListener("click", onClick, true);
    removeEventListener("pointerdown", swallow, true);
    removeEventListener("mousedown", swallow, true);
    removeEventListener("keydown", onKey, true);
    try { layer.hidePopover(); } catch {}
    host.remove();
    delete window.__herdrGrab;
  }
  // This session's token (web.rs fills it in); a local, never on window.
  const NONCE = "__HERDR_NONCE__";
  const post = (m) => window.ipc.postMessage(JSON.stringify({ ...m, nonce: NONCE }));
  const swallow = (e) => { e.preventDefault(); e.stopPropagation(); };
  const onMove = (e) => show(hit(e.clientX, e.clientY));
  const onClick = (e) => {
    swallow(e);
    // Only a real click picks: a page script can dispatch fake ones.
    if (!e.isTrusted) return;
    const el = hit(e.clientX, e.clientY) || current;
    if (!el) return;
    const grab = capture(el);
    stop();
    // A frame later the overlay is gone from the screen: then the app snapshots.
    requestAnimationFrame(() => requestAnimationFrame(() => post({ grab })));
  };
  const onKey = (e) => { if (e.key === "Escape" && e.isTrusted) { swallow(e); stop(); post({ grab_cancel: true }); } };

  addEventListener("pointermove", onMove, true);
  addEventListener("pointerdown", swallow, true);
  addEventListener("mousedown", swallow, true);
  addEventListener("click", onClick, true);
  addEventListener("keydown", onKey, true);
  window.__herdrGrab = { stop };
})();
