// SPIKE: a browser toolbar for a `{ "web": url }` block.
// props: { web: "<block id>" }. Reads the page from shared("web")[id]
// ({ url, title, loading, focused, design, grab }), drives it with web.* commands.
//
// Three states: browsing (address bar), picking (design mode on: click an
// element in the page), composing (a grab waits for your instruction; Enter
// sends both to an agent, Esc or × drops it).
import { View, div, svg } from "gpui-kit";
import { h_flex, InputState, Input } from "gpui-base";
import { run, props, shared, setShared, theme, hc, notify } from "herdr";
import { pickTarget, grabLabel, formatGrab } from "./grab.js";

const ICON = (name) => svg(`mods/web-toolbar/${name}.svg`);

// What you typed in the address bar -> what the page loads.
// The app adds https:// to anything without a scheme, so "github.com" works as is.
export function toUrl(text) {
  // TODO(you): decide when typed text is a search instead of an address.
  return text.trim();
}

export default class WebToolbar extends View {
  init(_props, cx) {
    this.id = props()?.web ?? "page";
    this.editing = false;
    this.input = InputState.new({ value: this.page().url ?? "" });
    this.input.on("focus", (_e, cx) => {
      this.editing = true;
      const v = this.input.value();
      this.input.set_selected_range({ start: 0, end: v.length });
      cx.notify();
    });
    this.input.on("blur", (_e, cx) => { this.editing = false; this.syncUrl(); cx.notify(); });
    this.input.on("submit", (_e, cx) => {
      const url = toUrl(this.input.value());
      if (url) run("web.navigate", { id: this.id, url });
      // Typing is done: the page gets the keyboard.
      run("web.focus", { id: this.id });
      cx.notify();
    });
    this.shown = null;

    // Composing: what to do with the picked element.
    this.ask = InputState.new({ value: "", placeholder: "Describe the change…" });
    this.ask.on("submit", (_e, cx) => this.send(cx));
    this.ask.on("change", (_e, cx) => cx.notify());
    this.composingAt = null;
    this.sending = false;
  }

  page() {
    return shared("web")?.[this.id] ?? {};
  }

  // Shows the page's address, unless the person is typing over it.
  syncUrl() {
    const url = this.page().url ?? "";
    if (!this.editing && url !== this.shown) {
      this.shown = url;
      this.input.set_value(url);
    }
  }

  // Drops the grab (shared, so every copy of the toolbar agrees).
  clearGrab() {
    const all = { ...(shared("web") ?? {}) };
    all[this.id] = { ...(all[this.id] ?? {}), grab: null };
    setShared("web", all);
    this.ask.set_value("");
    this.composingAt = null;
  }

  send(cx) {
    const grab = this.page().grab;
    const target = pickTarget();
    const instruction = this.ask.value();
    if (!grab || !target || !instruction.trim() || this.sending) return;
    this.sending = true;
    cx.notify();
    cx.spawn(async (cx) => {
      const r = await hc(["send", target.address, formatGrab(instruction, grab)]).catch((e) => ({ code: 1, stderr: String(e) }));
      this.sending = false;
      if (r.code === 0) {
        this.clearGrab();
        notify({ title: `Sent to ${target.name}`, body: grabLabel(grab), level: "info", timeout: 3000 });
      } else {
        notify({ title: "Couldn't send the element", body: (r.stderr || r.stdout || "").trim().slice(0, 300), level: "error" });
      }
      cx.notify();
    });
  }

  button(id, icon, command, t, args = {}, active = false) {
    return div().id(`wt-${id}`).size(26).flex().items_center().justify_center().rounded(6)
      .text_color(active ? t.accent_foreground : t.muted)
      .when(active, (el) => el.bg(t.accent))
      .when(!active, (el) => el.hover((s) => s.bg(t.hover).text_color(t.foreground)))
      .on_mouse_down("left", (_e, cx) => cx.stop_propagation())
      .on_click(() => run(command, { id: this.id, ...args }))
      .child(ICON(icon).size(15));
  }

  field(t, focused, children) {
    return h_flex().flex_1().min_w_0().h(28).px(10).gap(8).items_center().rounded(8).border_1()
      .border_color(focused ? t.accent : t.border).bg(t.background).children(children);
  }

  addressBar(t, p) {
    return this.field(t, this.editing, [
      p.loading ? div().size(6).rounded(3).bg(t.working).loop("opacity", { from: 0.3, to: 1, period: 700 }) : null,
      Input.new(this.input).flex_1().min_w_0().text_size(12.5).text_color(t.foreground),
      !this.editing && p.title ? div().max_w(220).truncate().text_size(11.5).text_color(t.dim).child(p.title) : null,
    ].filter(Boolean));
  }

  pickingBar(t) {
    return this.field(t, true, [
      div().size(6).rounded(3).bg(t.accent).loop("opacity", { from: 0.3, to: 1, period: 900 }),
      div().flex_1().truncate().text_size(12.5).text_color(t.muted).child("Click an element on the page  ·  Esc cancels"),
    ]);
  }

  composeBar(t, grab) {
    const target = pickTarget();
    const ready = Boolean(target && this.ask.value().trim()) && !this.sending;
    return this.field(t, true, [
      div().flex_none().max_w(200).truncate().px(6).py(1).rounded(5).bg(t.selected).text_size(11.5).text_color(t.foreground).child(grabLabel(grab)),
      Input.new(this.ask).flex_1().min_w_0().text_size(12.5).text_color(t.foreground),
      div().flex_none().max_w(140).truncate().text_size(11).text_color(target ? t.dim : t.blocked)
        .child(target ? `→ ${target.name}` : "no agent to send to"),
      div().id("wt-send").flex_none().size(22).flex().items_center().justify_center().rounded(5)
        .text_color(ready ? t.accent_foreground : t.dim).when(ready, (el) => el.bg(t.accent))
        .on_mouse_down("left", (_e, cx) => cx.stop_propagation())
        .on_click((_e, cx) => this.send(cx))
        .child(ICON("send").size(13)),
      div().id("wt-drop").flex_none().size(22).flex().items_center().justify_center().rounded(5)
        .text_color(t.muted).hover((s) => s.bg(t.hover).text_color(t.foreground))
        .on_mouse_down("left", (_e, cx) => cx.stop_propagation())
        .on_click((_e, cx) => { this.clearGrab(); cx.notify(); })
        .child(ICON("close").size(12)),
    ]);
  }

  render() {
    const t = theme();
    const p = this.page();
    this.syncUrl();
    const grab = p.grab;
    // A new grab takes the keyboard so you can type what to change.
    if (grab && this.composingAt !== grab.at) {
      this.composingAt = grab.at;
      this.ask.set_value("");
      this.ask.focus();
    }
    const middle = grab ? this.composeBar(t, grab) : p.design ? this.pickingBar(t) : this.addressBar(t, p);
    return h_flex().id("root").size_full().px(8).gap(4).items_center().bg(t.chrome)
      .on_key_down((e, cx) => { if (grab && e.key === "escape") { this.clearGrab(); cx.stop_propagation(); cx.notify(); } })
      .child(this.button("back", "back", "web.back", t))
      .child(this.button("forward", "forward", "web.forward", t))
      .child(p.loading ? this.button("stop", "stop", "web.stop", t) : this.button("reload", "reload", "web.reload", t))
      .child(middle)
      .child(this.button("design", "design", "web.design", t, {}, Boolean(p.design)))
      .child(this.button("inspect", "inspect", "web.devtools", t));
  }
}
