// SPIKE: what agents can do in a web block's page (web.rs runs it per command:
// `(this file)(op, args)`). Always returns one JSON string: wry can only hand
// back values it can serialize, and panics on others.
//
// Elements are named by `ref` ("e12", from web.read's tree) or by `selector`.
// Refs stay valid while the element is in the page; a navigation starts over.
(function (op, args) {
  const MAX_TEXT = 100000, MAX_NODES = 600;
  const reply = (v) => JSON.stringify(v);
  try {
    // Refs live with the page, so successive commands share them.
    const store = (window[Symbol.for("herdr.refs")] ??= { next: 1, byRef: new Map(), byEl: new WeakMap() });
    const refOf = (el) => {
      let r = store.byEl.get(el);
      if (!r) { r = "e" + store.next++; store.byEl.set(el, r); store.byRef.set(r, new WeakRef(el)); }
      return r;
    };
    const find = () => {
      if (args.ref) {
        const el = store.byRef.get(args.ref)?.deref();
        if (!el || !el.isConnected) throw new Error(`no element ${args.ref} on the page now: web.read again`);
        return el;
      }
      if (args.selector) {
        const el = document.querySelector(args.selector);
        if (!el) throw new Error(`nothing matches ${args.selector}`);
        return el;
      }
      throw new Error("give ref or selector");
    };
    const clip = (s, n) => (s = String(s ?? "").replace(/\s+/g, " ").trim()).length > n ? s.slice(0, n) + "…" : s;
    const visible = (el) => {
      const r = el.getBoundingClientRect();
      if (r.width === 0 && r.height === 0) return false;
      const cs = getComputedStyle(el);
      return cs.visibility !== "hidden" && cs.display !== "none";
    };
    const role = (el) => {
      const explicit = el.getAttribute("role");
      if (explicit) return explicit;
      const t = el.tagName.toLowerCase();
      if (t === "a" && el.hasAttribute("href")) return "link";
      if (t === "button" || (t === "input" && ["button", "submit", "reset"].includes(el.type))) return "button";
      if (t === "input" && el.type === "checkbox") return "checkbox";
      if (t === "input" && el.type === "radio") return "radio";
      if (t === "input" || t === "textarea") return "textbox";
      if (t === "select") return "combobox";
      if (/^h[1-6]$/.test(t)) return "heading";
      if (t === "img") return "img";
      if (el.isContentEditable) return "textbox";
      return null;
    };
    const name = (el) => clip(
      el.getAttribute("aria-label") || el.getAttribute("alt") || el.getAttribute("title") ||
      el.labels?.[0]?.innerText || el.getAttribute("placeholder") ||
      el.innerText || el.value || "", 120);
    const describe = (el) => ({ ref: refOf(el), role: role(el) ?? el.tagName.toLowerCase(), name: name(el) });
    const page = () => ({ url: location.href, title: document.title });

    switch (op) {
      case "read": {
        if (args.mode !== "tree") {
          return reply({ ...page(), text: clip(document.body?.innerText ?? "", MAX_TEXT) });
        }
        const nodes = [];
        const sel = "a[href],button,input,textarea,select,[role],[contenteditable=''],[contenteditable=true],h1,h2,h3,h4,h5,h6,img[alt],summary,label";
        for (const el of document.querySelectorAll(sel)) {
          if (nodes.length >= MAX_NODES) break;
          if (!visible(el)) continue;
          const n = describe(el);
          if (el.tagName === "A") n.href = el.href;
          if (/^H[1-6]$/.test(el.tagName)) n.level = Number(el.tagName[1]);
          if ("value" in el && el.type !== "password" && el.tagName !== "BUTTON") n.value = clip(el.value, 200);
          if (el.type === "password") n.password = true;
          if ("checked" in el && (el.type === "checkbox" || el.type === "radio")) n.checked = el.checked;
          if (el.disabled) n.disabled = true;
          if (el === document.activeElement) n.focused = true;
          nodes.push(n);
        }
        return reply({ ...page(), nodes, truncated: nodes.length >= MAX_NODES });
      }
      case "click": {
        const el = find();
        el.scrollIntoView({ block: "center", inline: "center" });
        const r = el.getBoundingClientRect();
        const at = { bubbles: true, cancelable: true, composed: true, clientX: r.x + r.width / 2, clientY: r.y + r.height / 2, button: 0 };
        el.dispatchEvent(new PointerEvent("pointerdown", at));
        el.dispatchEvent(new MouseEvent("mousedown", at));
        el.focus();
        el.dispatchEvent(new PointerEvent("pointerup", at));
        el.dispatchEvent(new MouseEvent("mouseup", at));
        el.click();
        return reply({ ...page(), clicked: describe(el) });
      }
      case "type": {
        const el = find();
        el.scrollIntoView({ block: "center" });
        el.focus();
        const text = String(args.text ?? "");
        if (el.isContentEditable) {
          if (args.clear !== false) document.execCommand("selectAll");
          document.execCommand("insertText", false, text);
        } else if ("value" in el) {
          // The prototype's setter: frameworks (React) watch it, not the property.
          const proto = el instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype
            : el instanceof HTMLSelectElement ? HTMLSelectElement.prototype : HTMLInputElement.prototype;
          const set = Object.getOwnPropertyDescriptor(proto, "value").set;
          set.call(el, (args.clear === false ? el.value : "") + text);
          el.dispatchEvent(new Event("input", { bubbles: true }));
          el.dispatchEvent(new Event("change", { bubbles: true }));
        } else {
          const d = describe(el);
          throw new Error(`${d.role} ${d.ref} doesn't take text`);
        }
        if (args.submit) {
          const key = { key: "Enter", code: "Enter", keyCode: 13, which: 13, bubbles: true, cancelable: true };
          const go = el.dispatchEvent(new KeyboardEvent("keydown", key));
          el.dispatchEvent(new KeyboardEvent("keyup", key));
          if (go && el.form) el.form.requestSubmit();
        }
        return reply({ ...page(), typed: describe(el) });
      }
      case "key": {
        const el = document.activeElement ?? document.body;
        const key = String(args.key ?? "");
        if (!key) throw new Error("give key, such as Enter, Escape, ArrowDown or a");
        const ev = { key, code: key.length === 1 ? "Key" + key.toUpperCase() : key, bubbles: true, cancelable: true };
        el.dispatchEvent(new KeyboardEvent("keydown", ev));
        el.dispatchEvent(new KeyboardEvent("keyup", ev));
        return reply({ ...page(), key, at: el === document.body ? null : describe(el) });
      }
      case "scroll": {
        if (args.ref || args.selector) find().scrollIntoView({ block: "center" });
        else if (args.to === "top") scrollTo(0, 0);
        else if (args.to === "bottom") scrollTo(0, document.documentElement.scrollHeight);
        else scrollBy(0, Number(args.by ?? innerHeight * 0.8));
        return reply({ ...page(), scrollY: Math.round(scrollY), height: document.documentElement.scrollHeight, viewport: innerHeight });
      }
      default:
        throw new Error(`unknown op ${op}`);
    }
  } catch (e) {
    return reply({ error: String(e && e.message || e) });
  }
})
