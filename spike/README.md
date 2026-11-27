# Web block spike (macOS)

A native web view (wry / WKWebView) inside Herdr's window: as a layout block
(`{ "web": "https://…" }`) or as a pane beside a terminal. It shows web pages and
local HTML files, and agents can read and drive it once the person allows them.
macOS only; a work in progress, not ready for review.

## Run it

You need Rust and macOS. The spike runs in its own home and core under
`/var/tmp/herdr-web`, so your real Herdr, its terminals and settings are never touched.

```sh
cargo build
scripts/web-spike                 # opens the "Herdr Web" window
scripts/web-spike do <command>    # talks to that window, like `herdr-app do`
```

The first run copies `spike/home/` into the spike's home (existing files are kept).
To start over, quit the window and delete `/var/tmp/herdr-web`.

### The web layout

`layouts/web.json` puts a browser beside the workspace: a `web-toolbar` mod over a
web block (`page`, showing herdr.dev). It was built from a personal layout and uses
two mods that don't ship with Herdr, `rex-tabs` and `crew`. Without them, edit
`/var/tmp/herdr-web/home/layouts/web.json` and change `"mod": "rex-tabs"` to `"tabs"`
and `"mod": "crew"` to `"agents"`. Then:

```sh
scripts/web-spike do layout.switch --name web
```

Any layout can show a page: add a block with `"content": { "web": "<url>" }`.

## Try it

```sh
# A page as a pane beside the focused terminal (a url, or a local .html file)
scripts/web-spike do pane.split --direction right --web https://herdr.dev
scripts/web-spike do pane.split --direction right --web ~/notes/spec.html

# Drive the page in the web layout's block (id "page"; default: the last one shown)
scripts/web-spike do web.navigate --id page --url github.com
scripts/web-spike do web.back --id page
```

Cmd-click an `.html` path in a terminal to open it as a pane (the `web-open` mod).
Click a page to give it the keyboard; Herdr's own keys (`global` and the shown
layout's section in keys.json) still reach Herdr.

## Commands

Page: `web.navigate {url}`, `web.back`, `web.forward`, `web.reload`, `web.stop`,
`web.focus`, `web.devtools`, `web.design` (pick an element on the page and send it to
an agent from the toolbar). Each takes an optional `id`, the block or pane.

Agents: `web.read {mode?}` (the page's text, or `"tree"`: links, buttons, fields and
headings, each with a `ref`), `web.click {ref | selector}`,
`web.type {text, ref | selector, submit?, clear?}`, `web.key {key}`,
`web.scroll {by? | to? | ref | selector}`, `web.screenshot {path?}`,
`web.forget` (takes agents' access back).

Mods read each page's state from shared `"web"`:
`{ [id]: { url, title, loading, focused, design, grab } }`.

## How it keeps pages contained

- **Consent:** the first agent command on a site shows a native sheet in the window,
  and the command waits for the answer. A yes lasts for that block and site until
  Herdr quits. The check uses the page's live URL, and the script runs only if the
  page is still on that site (or folder).
- **Local files** load as `herdr-file:` and can read only their own folder (a file
  directly in your home folder reads only itself). They can't reach the network
  (CSP and a WebKit content rule list), navigate to the web or open windows.
- **WebRTC is off** for every page (a private WebKit preference), so calls and
  screen sharing don't work.
- Sign-ins persist in Herdr's own website data store, apart from Safari's.

## Known limits

- macOS only. wry would give WebView2 on Windows; Linux is open.
- Local pages can't load from a CDN, and back from a local page to a web page is refused.
- Agent events are synthetic, so some sites ignore them. This isn't CDP.
- Under a Herdr dialog or popup the page shows a picture of itself. `herdr-app shot`
  doesn't capture native pages or the consent sheet.
- `third_party_notices_are_current_and_complete` fails: the new dependencies need
  `just notices`.

## Where the code is

- `src/web.rs`: placement, keyboard handoff, local files, consent, snapshots.
- `src/web_agent.js` and `src/web_grab.js`: the in-page agent helper and design-mode picker.
- `src/window.rs`, `src/layout.rs`, `src/keys.rs`, `src/commands.rs`: the block, panes,
  key handoff and commands.
- `spike/home/`: the `web-toolbar` and `web-open` mods and the `web` layout.
