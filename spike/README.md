# Web block spike (macOS)

A native web view (wry / WKWebView) as a layout block: `{ "web": "https://…" }`.

- `src/web.rs`: placement, per-page container view (devtools stay inside the block),
  keyboard handoff, design-mode grabs and snapshots.
- `src/web_grab.js`: the in-page element picker for design mode.
- `spike/home/`: the `web-toolbar` mod (address bar, back/forward/reload, devtools,
  design mode, send a picked element to an agent) and a `web` layout.

Run it in an isolated home and core (never your real Herdr):

```sh
cargo build
scripts/web-spike
```

`layouts/web.json` is built from a personal layout and expects the `rex-tabs` and
`crew` mods; copy them into `/var/tmp/herdr-web/home/mods/` or point the layout
at the shipped `tabs` and `agents`.

Commands: `web.navigate {url, id?}`, `web.back`, `web.forward`, `web.reload`,
`web.devtools`, `web.design`, `web.focus`, `web.stop`.
Page state for mods: shared `"web"` → `{ [block id]: { url, title, loading, focused, design, grab } }`.
