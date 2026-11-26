//! SPIKE: `{ "web": "<url>" }` blocks — a native system web view (WKWebView on
//! macOS, through wry) placed over the block's bounds.
//!
//! GPUI draws the whole window on one GPU surface and a native view always
//! sits above it, so this module keeps the web views honest every frame:
//! - `begin_frame` forgets which views were drawn,
//! - each web block's paint calls `place` with its bounds (creating the view
//!   the first time),
//! - `end_frame` hides every view that wasn't drawn this frame (another
//!   layout, a hidden node, the other child of a stack), every view while a
//!   dialog is open, and any view under another overlay.
//!
//! Page state is published as shared `"web"`:
//! `{ "<block id>": { url, title, loading, focused, design, grab } }`.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use gpui::{Bounds, Pixels, Window};
use serde_json::{json, Value};
use wry::dpi::{LogicalPosition, LogicalSize};
use wry::{PageLoadEvent, Rect, WebView, WebViewBuilder, WebViewBuilderExtDarwin};

struct Entry {
    // Dropped before its container (field order): the page leaves, then its box.
    view: WebView,
    container: container::Container,
    /// The url the layout asked for; a change in the layout loads the new one.
    layout_url: String,
    bounds: Option<Bounds<Pixels>>,
    visible: bool,
    /// Hidden under an overlay: the block shows a picture of the page instead.
    covered: bool,
}

/// Herdr's WebKit data store (macOS 14+): cookies and sign-ins for every block.
const STORE: [u8; 16] = [0x6e, 0x3a, 0x1c, 0x58, 0x9b, 0x42, 0x4d, 0x07, 0xa1, 0x6f, 0x2e, 0xd4, 0x83, 0x5b, 0xc9, 0x10];

thread_local! {
    /// Pictures of covered pages, by block (`None`: the snapshot failed).
    static SHOTS: RefCell<HashMap<String, Option<std::sync::Arc<gpui::Image>>>> = RefCell::new(HashMap::new());
    /// (block, site) pairs the person let agents drive, until Herdr restarts.
    static ALLOWED: RefCell<HashSet<(String, String)>> = RefCell::new(HashSet::new());
    /// Sites an allow sheet is up for.
    static ASKING: RefCell<HashSet<(String, String)>> = RefCell::new(HashSet::new());
}

/// What one consent covers: a web origin, or a local folder.
fn site_of(id: &str, url: &str) -> String {
    if url.starts_with(LOCAL) {
        let root = ROOTS.lock().unwrap().as_ref().and_then(|r| r.get(id).cloned());
        return root.map(|r| r.display().to_string()).unwrap_or_else(|| "local files".into());
    }
    url::Url::parse(url).map(|u| u.origin().ascii_serialization()).unwrap_or_else(|_| url.to_owned())
}

#[derive(Default)]
pub struct Web {
    views: HashMap<String, Entry>,
    seen: HashSet<String>,
    /// Overlay cards drawn this frame: a web view under any of them hides.
    covers: Vec<Bounds<Pixels>>,
    /// The block a `web.*` command without `id` acts on: the last one placed.
    last: Option<String>,
}

const TITLE_REPORTER: &str = r#"(() => {
  let last = null;
  const send = () => { const t = document.title; if (t !== last) { last = t; window.ipc.postMessage(JSON.stringify({ title: t })); } };
  const watch = () => { send(); new MutationObserver(send).observe(document.head || document.documentElement, { subtree: true, childList: true, characterData: true }); };
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", watch); else watch();
  window.addEventListener("pageshow", send);
})();"#;

fn rect(b: Bounds<Pixels>) -> Rect {
    Rect {
        position: LogicalPosition::new(f32::from(b.origin.x) as f64, f32::from(b.origin.y) as f64)
            .into(),
        size: LogicalSize::new(
            f32::from(b.size.width).max(1.0) as f64,
            f32::from(b.size.height).max(1.0) as f64,
        )
        .into(),
    }
}

/// Merges one field of one block's page state into shared `"web"`.
fn publish(id: &str, key: &str, value: Value) {
    let mut all = crate::mods::shared("web")
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    let entry = all
        .as_object_mut()
        .unwrap()
        .entry(id.to_owned())
        .or_insert_with(|| json!({}));
    if entry.get(key) == Some(&value) {
        return;
    }
    entry[key] = value;
    crate::mods::set_shared("web".into(), all);
    if matches!(key, "title" | "url") {
        crate::mods::request(crate::mods::Request::WebPage);
    }
}

const GRAB_SCRIPT: &str = include_str!("web_grab.js");
const AGENT_SCRIPT: &str = include_str!("web_agent.js");
/// Commands that answer later, from the page (`Web::agent`).
pub const AGENT_COMMANDS: &[&str] = &["web.read", "web.click", "web.type", "web.key", "web.scroll", "web.screenshot"];
const GRAB_STOP: &str = "window.__herdrGrab && window.__herdrGrab.stop()";

/// Local files load as `herdr-file://localhost/<path>`.
const LOCAL: &str = "herdr-file://";

/// Each block's local folder: the folder of the file it was last sent to.
/// `herdr-file:` serves only what's inside it, so a page can load its own CSS
/// and scripts but nothing else on the disk. (The handler runs off the main thread.)
static ROOTS: std::sync::Mutex<Option<HashMap<String, std::path::PathBuf>>> = std::sync::Mutex::new(None);

fn file_of(url: &str) -> Option<std::path::PathBuf> {
    let rest = url.strip_prefix(LOCAL)?;
    let path = rest.find('/').map(|i| &rest[i..])?;
    url::Url::parse(&format!("file://{path}")).ok()?.to_file_path().ok()
}

/// Remembers the folder a block's page may read, when `url` is a local file.
/// A file right in the home folder (or above it) gets only itself: its page
/// never reads the whole home.
fn allow_folder(id: &str, url: &str) {
    let home = std::env::var_os("HOME").and_then(|h| std::path::Path::new(&h).canonicalize().ok());
    let folder = file_of(url).and_then(|f| f.canonicalize().ok()).and_then(|f| {
        let dir = f.parent()?.to_path_buf();
        Some(if dir.parent().is_none() || home.as_ref().is_some_and(|h| h.starts_with(&dir)) { f } else { dir })
    });
    let mut roots = ROOTS.lock().unwrap();
    let roots = roots.get_or_insert_with(HashMap::new);
    match folder {
        Some(f) => roots.insert(id.to_owned(), f),
        None => roots.remove(id),
    };
}

/// Local pages load only their own folder's files: no network, so what they
/// read stays on the machine.
const LOCAL_CSP: &str = "default-src herdr-file: data: blob: 'unsafe-inline' 'unsafe-eval'; form-action 'none'; base-uri 'none'";

/// A local page stays local: it can't carry what it read off to a web address
/// by navigating there (Herdr's own web.navigate takes the folder back first).
fn may_navigate(id: &str, url: &str) -> bool {
    url.starts_with(LOCAL) || url.starts_with("about:") || ROOTS.lock().unwrap().as_ref().is_none_or(|r| !r.contains_key(id))
}

/// Answers a `herdr-file:` request from block `id`'s page.
fn serve_local(id: &str, req: &wry::http::Request<Vec<u8>>) -> wry::http::Response<std::borrow::Cow<'static, [u8]>> {
    let status = |code: u16| wry::http::Response::builder().status(code).body(std::borrow::Cow::Borrowed(&[][..])).unwrap();
    // A web page (an iframe in a local one) can't read local files.
    if req.headers().get("origin").is_some_and(|o| !o.as_bytes().starts_with(b"herdr-file:")) {
        return status(403);
    }
    let uri = req.uri().to_string();
    let root = ROOTS.lock().unwrap().as_ref().and_then(|r| r.get(id).cloned());
    let Some(path) = file_of(&uri).and_then(|p| p.canonicalize().ok()) else { return status(404) };
    if !root.is_some_and(|r| path.starts_with(r)) {
        return status(403);
    }
    let Ok(bytes) = std::fs::read(&path) else { return status(404) };
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    let mime = match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "wasm" => "application/wasm",
        "txt" | "md" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    };
    wry::http::Response::builder()
        .header("Content-Type", mime)
        .header("Content-Security-Policy", LOCAL_CSP)
        .body(std::borrow::Cow::Owned(bytes))
        .unwrap()
}

/// What a web block loads for typed text: an http(s) address, normalised
/// (spaces and other unsafe characters encoded), or a local file (`/…`, `~/…`,
/// `file://…`) served through `herdr-file:`. A bare host gets https://.
/// Other schemes (javascript:, data:) are refused, and so is anything
/// that isn't a valid address: wry panics on a URL macOS can't parse.
pub fn address(text: &str) -> Result<String, String> {
    let text = text.trim();
    let local = if text.starts_with(LOCAL) {
        Some(file_of(text).ok_or_else(|| format!("not a file address: {text}"))?)
    } else if let Some(rest) = text.strip_prefix("~/") {
        std::env::var_os("HOME").map(|h| std::path::Path::new(&h).join(rest))
    } else if text.starts_with('/') {
        Some(std::path::PathBuf::from(text))
    } else if text.get(..7).is_some_and(|s| s.eq_ignore_ascii_case("file://")) {
        Some(url::Url::parse(text).ok().and_then(|u| u.to_file_path().ok()).ok_or_else(|| format!("not a file address: {text}"))?)
    } else {
        None
    };
    if let Some(path) = local {
        if !path.is_file() {
            return Err(format!("no file {}", path.display()));
        }
        // The real path: the consent check compares it with the page's folder.
        let path = path.canonicalize().map_err(|e| format!("{}: {e}", path.display()))?;
        let file = url::Url::from_file_path(&path).map_err(|_| format!("not a file path: {}", path.display()))?;
        return Ok(format!("{LOCAL}localhost{}", file.path()));
    }
    let scheme = text.split_once(':').map(|(s, _)| s).filter(|s| {
        let mut c = s.chars();
        c.next().is_some_and(|f| f.is_ascii_alphabetic()) && c.all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c))
    });
    let full = match scheme.map(str::to_ascii_lowercase).as_deref() {
        Some("http" | "https") => text.to_owned(),
        // "localhost:3000" reads as a scheme but is a host and port.
        Some(_) if !text.contains("//") && text.split_once(':').is_some_and(|(_, rest)| rest.split('/').next().is_some_and(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))) => format!("https://{text}"),
        Some(s) => return Err(format!("a web view opens http(s) addresses and local files, not {s}:")),
        None => format!("https://{text}"),
    };
    let parsed = url::Url::parse(&full).map_err(|e| format!("not a web address: {text} ({e})"))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none_or(|h| h.contains(' ')) {
        return Err(format!("not a web address: {text}"));
    }
    Ok(parsed.into())
}

fn design_on(id: &str) -> bool {
    crate::mods::shared("web").and_then(|w| w.get(id)?.get("design")?.as_bool()).unwrap_or(false)
}

thread_local! {
    /// Each design session's token: the picker posts it back, so frames and
    /// scripts that never saw this session's picker can't post a grab.
    static NONCES: RefCell<HashMap<String, String>> = RefCell::new(HashMap::new());
}

fn new_nonce() -> String {
    use std::hash::{BuildHasher, Hasher};
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let mut parts = [0u64; 2];
    for p in &mut parts {
        // RandomState is seeded from the OS's randomness.
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(now);
        *p = h.finish();
    }
    format!("{:016x}{:016x}", parts[0], parts[1])
}

/// Ends the design session if `m` carries its token (so the token works once).
fn take_nonce(id: &str, m: &serde_json::Map<String, Value>) -> bool {
    let Some(sent) = m.get("nonce").and_then(Value::as_str) else { return false };
    NONCES.with_borrow_mut(|n| {
        if n.get(id).is_some_and(|want| want == sent) {
            n.remove(id);
            true
        } else {
            false
        }
    })
}

fn save_png(path: std::path::PathBuf, png: Vec<u8>) -> Option<std::path::PathBuf> {
    std::fs::create_dir_all(path.parent()?).ok()?;
    std::fs::write(&path, png).ok()?;
    Some(path)
}

/// Messages a page posts (`window.ipc.postMessage`). Pages are untrusted: any
/// page or frame can post anything, so a grab counts only while design mode is
/// on and only with this session's token.
fn on_ipc(id: &str, body: &str) {
    if body.len() > 64 * 1024 {
        return;
    }
    let Ok(Value::Object(m)) = serde_json::from_str::<Value>(body) else { return };
    if let Some(t) = m.get("title") {
        publish(id, "title", json!(clamp_line(t.as_str().unwrap_or(""), 300)));
    }
    if m.contains_key("grab_cancel") && design_on(id) && take_nonce(id, &m) {
        publish(id, "design", json!(false));
    }
    if let Some(grab) = m.get("grab")
        && design_on(id)
        && take_nonce(id, &m)
    {
        publish(id, "design", json!(false));
        // You type what to change next, in Herdr's toolbar: the keyboard comes back.
        container::give_back();
        let Some(mut grab) = clamp_grab(grab) else { return };
        let r = &grab["rect"];
        let rect = [r["x"].as_f64(), r["y"].as_f64(), r["width"].as_f64(), r["height"].as_f64()];
        let block = id.to_owned();
        let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
        let path = crate::mods::app_dir().join("data/web-grabs").join(format!("{id}-{at}.png"));
        let done = move |shot: Option<std::path::PathBuf>| {
            grab["screenshot"] = shot.map(|p| json!(p.to_string_lossy())).unwrap_or(Value::Null);
            grab["at"] = json!(at as u64);
            publish(&block, "grab", grab);
        };
        match rect {
            [Some(x), Some(y), Some(w), Some(h)] if w >= 1.0 && h >= 1.0 => container::snapshot(id, [x, y, w, h], move |png| done(png.and_then(|png| save_png(path, png)))),
            _ => done(None),
        }
    }
}

/// Page text bound for a terminal: no control characters (ESC sequences, CR,
/// C1, bidi overrides), at most `max` characters. Newlines and tabs stay.
fn clamp_str(s: &str, max: usize) -> String {
    let bidi = |c: char| matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}');
    let clean: String = s.chars().filter(|&c| c == '\n' || c == '\t' || !(c.is_control() || bidi(c))).collect();
    if clean.chars().count() <= max { clean } else { clean.chars().take(max).collect::<String>() + "…" }
}

/// One line of page text: `clamp_str` with every run of whitespace a single space.
fn clamp_line(s: &str, max: usize) -> String {
    clamp_str(&s.split_whitespace().collect::<Vec<_>>().join(" "), max)
}

/// Keeps only the fields design mode knows, each bounded: the page wrote this.
fn clamp_grab(g: &Value) -> Option<Value> {
    let str_of = |v: &Value, max| clamp_line(v.as_str().unwrap_or(""), max);
    let num = |v: &Value| v.as_f64().filter(|n| n.is_finite()).unwrap_or(0.0);
    let page = g.get("page")?;
    let rect = g.get("rect")?;
    let styles: serde_json::Map<String, Value> = g
        .get("styles")
        .and_then(Value::as_object)
        .map(|m| m.iter().take(40).map(|(k, v)| (clamp_line(k, 40), json!(str_of(v, 200)))).collect())
        .unwrap_or_default();
    let ancestors: Vec<Value> = g
        .get("ancestors")
        .and_then(Value::as_array)
        .map(|a| a.iter().take(8).map(|v| json!(str_of(v, 120))).collect())
        .unwrap_or_default();
    let source = g.get("source").filter(|s| s.is_object()).map(|s| {
        json!({ "file": str_of(&s["file"], 400), "line": s["line"].as_u64(), "component": s["component"].as_str().map(|c| clamp_line(c, 80)) })
    });
    let url = str_of(&page["url"], 2000);
    if !(url.starts_with("http://") || url.starts_with("https://") || url.starts_with("file://")) {
        return None;
    }
    Some(json!({
        "page": { "url": url, "title": str_of(&page["title"], 300) },
        "tag": str_of(&g["tag"], 40),
        "role": g["role"].as_str().map(|r| clamp_line(r, 40)),
        "text": str_of(&g["text"], 300),
        "selector": str_of(&g["selector"], 400),
        "html": clamp_str(g["html"].as_str().unwrap_or(""), 3000),
        "styles": styles,
        "ancestors": ancestors,
        "rect": { "x": num(&rect["x"]), "y": num(&rect["y"]), "width": num(&rect["width"]), "height": num(&rect["height"]) },
        "source": source,
    }))
}

impl Web {
    pub fn begin_frame(&mut self, layout: &str) {
        container::set_layout(layout);
        self.seen.clear();
        self.covers.clear();
    }

    pub fn cover(&mut self, b: Bounds<Pixels>) {
        self.covers.push(b);
    }

    /// Called from the block's paint: makes the view if needed and puts it here.
    pub fn place(&mut self, id: &str, layout_url: &str, b: Bounds<Pixels>, window: &Window) {
        self.seen.insert(id.to_owned());
        self.last = Some(id.to_owned());
        let url = &address(layout_url).unwrap_or_else(|_| layout_url.to_owned());
        if let Some(e) = self.views.get_mut(id) {
            if e.layout_url != layout_url {
                e.layout_url = layout_url.to_owned();
                allow_folder(id, url);
                let _ = e.view.load_url(url);
            }
            if e.bounds != Some(b) {
                e.bounds = Some(b);
                e.container.place(b);
            }
            return;
        }
        // The page lives in a box of its own the size of the block: WebKit docks
        // the inspector inside the page's parent, so that parent must be the block.
        let Some(container) = container::Container::new(window, b, id) else {
            tracing::warn!("web block {id}: no native window to attach to");
            return;
        };
        let size = Bounds { origin: gpui::Point::default(), size: b.size };
        let (load_id, title_id, nav_id) = (id.to_owned(), id.to_owned(), id.to_owned());
        allow_folder(id, url);
        let built = WebViewBuilder::new()
            .with_id(id)
            .with_url(url)
            .with_bounds(rect(size))
            .with_devtools(true)
            // Pages take the keyboard only when clicked (or by web.focus / design mode).
            .with_focused(false)
            // Sign-ins persist in Herdr's own store, apart from Safari's.
            .with_data_store_identifier(STORE)
            .with_custom_protocol("herdr-file".into(), |id, req| serve_local(id, &req))
            // Navigations include iframes (ads, embeds): the address comes only
            // from main-frame page loads below.
            .with_navigation_handler(move |u| may_navigate(&nav_id, &u))
            .with_new_window_req_handler(|_, _| wry::NewWindowResponse::Deny)
            .with_on_page_load_handler(move |ev, u| {
                // Left its local file: the folder isn't the page's to read any more.
                if !u.starts_with(LOCAL) {
                    allow_folder(&load_id, &u);
                }
                publish(&load_id, "url", json!(u));
                publish(
                    &load_id,
                    "loading",
                    json!(matches!(ev, PageLoadEvent::Started)),
                );
            })
            // The page reports its own title: on load, and whenever <title> changes
            // (WebKit's title callback misses some navigations).
            .with_initialization_script(TITLE_REPORTER)
            .with_ipc_handler(move |req| on_ipc(&title_id, req.body()))
            .build_as_child(&container);
        match built {
            Ok(view) => {
                container.fill_with(&view, id);
                publish(id, "url", json!(url));
                self.views.insert(
                    id.to_owned(),
                    Entry {
                        view,
                        container,
                        layout_url: layout_url.to_owned(),
                        bounds: Some(b),
                        visible: true,
                        covered: false,
                    },
                );
            }
            Err(e) => tracing::warn!("web block {id}: {e}"),
        }
    }

    /// After everything painted: hide what wasn't drawn, or everything while covered.
    /// A page under an overlay leaves a picture of itself behind, so the overlay
    /// looks drawn over the page instead of over a hole. WebKit can't picture a
    /// hidden page, so it stays up until its picture is ready (a frame or two).
    pub fn end_frame(&mut self, covered: bool) {
        for (id, e) in self.views.iter_mut() {
            let seen = self.seen.contains(id);
            let under = e.bounds.is_some_and(|b| self.covers.iter().any(|c| c.intersects(&b)));
            let cover = seen && (covered || under);
            if cover != e.covered {
                e.covered = cover;
                if cover {
                    let id = id.clone();
                    container::snapshot(&id.clone(), [0.0, 0.0, 1.0e7, 1.0e7], move |png| {
                        let image = png.map(|png| std::sync::Arc::new(gpui::Image::from_bytes(gpui::ImageFormat::Png, png)));
                        SHOTS.with_borrow_mut(|s| s.insert(id, image));
                        crate::mods::request(crate::mods::Request::WebPage);
                    });
                } else {
                    SHOTS.with_borrow_mut(|s| s.remove(id));
                }
            }
            let pictured = SHOTS.with_borrow(|s| s.contains_key(id));
            let show = seen && !(cover && pictured);
            if show != e.visible {
                e.visible = show;
                e.container.set_visible(show);
            }
        }
    }

    /// The picture a covered page left in its block.
    pub fn shot(&self, id: &str) -> Option<std::sync::Arc<gpui::Image>> {
        self.views.get(id).filter(|e| e.covered)?;
        SHOTS.with_borrow(|s| s.get(id).cloned().flatten())
    }

    /// Drops views whose block or pane is gone (hidden ones stay, like mods).
    pub fn retain(&mut self, alive: &HashSet<String>) {
        self.views.retain(|id, _| alive.contains(id));
        SHOTS.with_borrow_mut(|s| s.retain(|id, _| alive.contains(id)));
        if let Some(roots) = ROOTS.lock().unwrap().as_mut() {
            roots.retain(|id, _| alive.contains(id));
        }
    }

    fn id(&self, id: Option<&str>) -> Result<String, String> {
        id.map(str::to_owned).or_else(|| self.last.clone()).ok_or_else(|| "no web block is shown".into())
    }

    fn view(&self, id: Option<&str>) -> Result<&WebView, String> {
        let id = self.id(id)?;
        self.views
            .get(&id)
            .map(|e| &e.view)
            .ok_or_else(|| format!("no web block {id:?}"))
    }

    pub fn command(&self, name: &str, id: Option<&str>, url: Option<&str>) -> Result<(), String> {
        let v = self.view(id)?;
        let r = match name {
            "web.navigate" => {
                let url = address(url.ok_or("web.navigate needs url")?)?;
                allow_folder(&self.id(id)?, &url);
                v.load_url(&url)
            }
            "web.forget" => {
                let id = self.id(id)?;
                ALLOWED.with_borrow_mut(|a| a.retain(|(block, _)| *block != id));
                Ok(())
            }
            "web.back" => v.evaluate_script("history.back()"),
            "web.forward" => v.evaluate_script("history.forward()"),
            "web.reload" => v.reload(),
            "web.stop" => v.evaluate_script("window.stop()"),
            "web.design" => {
                let id = id.map(str::to_owned).or_else(|| self.last.clone()).unwrap_or_default();
                let on = !design_on(&id);
                publish(&id, "design", json!(on));
                if on {
                    // A fresh pick replaces a grab nobody sent yet.
                    publish(&id, "grab", Value::Null);
                    let _ = v.focus();
                    let nonce = new_nonce();
                    NONCES.with_borrow_mut(|n| n.insert(id.clone(), nonce.clone()));
                    v.evaluate_script(&GRAB_SCRIPT.replace("__HERDR_NONCE__", &nonce))
                } else {
                    NONCES.with_borrow_mut(|n| n.remove(&id));
                    v.evaluate_script(GRAB_STOP)
                }
            }
            "web.devtools" => {
                v.open_devtools();
                Ok(())
            }
            _ => return Err(format!("unknown web command {name}")),
        };
        r.map_err(|e| e.to_string())
    }

    /// What agents run (`web.read`, `web.click`, `web.type`, `web.key`,
    /// `web.scroll`, `web.screenshot`): the page answers later, so the reply
    /// goes to `reply` when it does. The person allows each site once per
    /// block, in a native sheet a mod or agent can't answer for them.
    pub fn agent(&self, name: &str, args: &Value, reply: std::sync::mpsc::Sender<Value>) {
        let fail = |reply: &std::sync::mpsc::Sender<Value>, e: String| {
            let _ = reply.send(json!({ "error": e }));
        };
        let id = args.get("id").and_then(Value::as_str);
        let v = match self.view(id) {
            Ok(v) => v,
            Err(e) => return fail(&reply, e),
        };
        let id = id.map(str::to_owned).or_else(|| self.last.clone()).unwrap_or_default();
        // The page as it is now, not as last reported: consent is for this site.
        let url = v.url().unwrap_or_default();
        let key = (id.clone(), site_of(&id, &url));
        if key.1 == "null" {
            return fail(&reply, format!("agents can't use this page ({url})"));
        }
        if !ALLOWED.with_borrow(|a| a.contains(&key)) {
            if !ASKING.with_borrow_mut(|a| a.insert(key.clone())) {
                return fail(&reply, format!("waiting for the person to allow agents on {}", key.1));
            }
            let cmd = json!({ "do": name, "args": args });
            let site = key.1.clone();
            return container::ask(&id, &site, move |yes| {
                ASKING.with_borrow_mut(|a| a.remove(&key));
                if !yes {
                    return fail(&reply, format!("the person didn't allow agents on {}", key.1));
                }
                ALLOWED.with_borrow_mut(|a| a.insert(key));
                // Runs again, now allowed.
                crate::mods::request(crate::mods::Request::Control { cmd, reply });
            });
        }
        if name == "web.screenshot" {
            let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
            let path = match args.get("path").and_then(Value::as_str) {
                Some(p) if std::path::Path::new(p).is_absolute() => std::path::PathBuf::from(p),
                Some(p) => return fail(&reply, format!("path must be absolute, not {p}")),
                None => crate::mods::app_dir().join("data/web-shots").join(format!("{id}-{at}.png")),
            };
            let page = crate::mods::shared("web").and_then(|w| w.get(&id).cloned()).unwrap_or_default();
            // The whole visible page: snapshot keeps the rect inside the view.
            container::snapshot(&id, [0.0, 0.0, 1.0e7, 1.0e7], move |png| {
                let _ = reply.send(match png.and_then(|png| save_png(path, png)) {
                    Some(p) => json!({ "path": p.to_string_lossy(), "url": page.get("url"), "title": page.get("title") }),
                    None => json!({ "error": "the page couldn't be captured" }),
                });
            });
            return;
        }
        let op = name.trim_start_matches("web.");
        let mut a = args.clone();
        if let Some(m) = a.as_object_mut() {
            m.remove("id");
        }
        let a = if a.is_object() { a } else { json!({}) };
        // The page may navigate before the script runs: it checks it's still on the allowed site.
        let still = if url.starts_with(LOCAL) {
            let Some(root) = ROOTS.lock().unwrap().as_ref().and_then(|r| r.get(&id).cloned()) else {
                return fail(&reply, "the page changed: web.read again".into());
            };
            // In the allowed folder (or the one allowed file), not just any local page.
            format!(
                "location.protocol === \"herdr-file:\" && ((p, r) => p === r || p.startsWith(r + \"/\"))(decodeURIComponent(location.pathname), {})",
                json!(root.to_string_lossy())
            )
        } else {
            format!("location.origin === {}", json!(key.1))
        };
        let script = format!("({still}) ? ({AGENT_SCRIPT})({}, {}) : JSON.stringify({{ error: \"the page changed: web.read again\" }})", json!(op), a);
        // wry wants a callback it can call more than once; it calls it once.
        let reply = std::sync::Mutex::new(Some(reply));
        let sent = v.evaluate_script_with_callback(&script, move |result| {
            let Some(reply) = reply.lock().ok().and_then(|mut r| r.take()) else { return };
            // The helper returns a JSON string, which wry encodes as JSON again.
            let answer = serde_json::from_str::<String>(&result)
                .ok()
                .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                .unwrap_or_else(|| json!({ "error": "the page didn't answer: it may still be loading, or blocks scripts" }));
            let _ = reply.send(answer);
        });
        if let Err(e) = sent {
            tracing::warn!("web agent script: {e}");
        }
    }

    /// Gives the page the keyboard (a click on it does this too, natively).
    pub fn focus(&self, id: Option<&str>) -> Result<(), String> {
        let id = id.map(str::to_owned).or_else(|| self.last.clone()).ok_or("no web block is shown")?;
        self.view(Some(&id))?.focus().map_err(|e| e.to_string())?;
        container::focused(Some(&id));
        Ok(())
    }

    /// Takes the keyboard back from whichever page has it.
    pub fn give_back(&self) {
        container::give_back();
    }
}

/// SPIKE (macOS): a plain NSView at a block's bounds, holding that block's page.
///
/// Also the keyboard handoff: a page with the keyboard gets keys from AppKit
/// directly, before GPUI. An event monitor (it runs before any view) sends
/// Herdr's own keys (`global` and the shown `layout:` section) back to GPUI's
/// view first, so tab, space, pane and app keys and the escape hatch keep
/// working; everything else (typing, copy, paste, find) stays with the page.
mod container {
    use block2::RcBlock;
    use gpui::{Bounds, Pixels, Window};
    use objc2::{MainThreadMarker, Message};
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2_app_kit::{NSAlert, NSAlertFirstButtonReturn, NSModalResponse, NSAutoresizingMaskOptions, NSBitmapImageFileType, NSBitmapImageRep, NSEvent, NSEventMask, NSEventType, NSImage, NSView};
    use objc2_foundation::{NSDictionary, NSError, NSString};
    use objc2_web_kit::{WKSnapshotConfiguration, WKWebView};
    use objc2_core_foundation::{CGPoint, CGRect, CGSize};
    use raw_window_handle::{
        AppKitWindowHandle, HandleError, HasWindowHandle, RawWindowHandle, WindowHandle,
    };
    use std::cell::{Cell, RefCell};
    use std::ptr::NonNull;
    use wry::WebViewExtMacOS;

    thread_local! {
        /// Every page box, with its block id.
        static BOXES: RefCell<Vec<(Retained<NSView>, String)>> = const { RefCell::new(Vec::new()) };
        /// GPUI's view: where Herdr's keys must land.
        static GPUI: RefCell<Option<Retained<NSView>>> = const { RefCell::new(None) };
        static LAYOUT: RefCell<String> = const { RefCell::new(String::new()) };
        /// The block whose page has the keyboard, as last told to mods.
        static FOCUSED: RefCell<Option<String>> = const { RefCell::new(None) };
        static MONITOR: Cell<bool> = const { Cell::new(false) };
        /// Each block's WKWebView, for snapshots.
        static PAGES: RefCell<std::collections::HashMap<String, Retained<WKWebView>>> = RefCell::new(Default::default());
    }

    /// A PNG of part of a page (`rect`: CSS px from its top-left), handed to `done`.
    pub fn snapshot(id: &str, rect: [f64; 4], done: impl FnOnce(Option<Vec<u8>>) + 'static) {
        let Some(page) = PAGES.with_borrow(|p| p.get(id).cloned()) else { return done(None) };
        let Some(mtm) = MainThreadMarker::new() else { return done(None) };
        let config = unsafe { WKSnapshotConfiguration::new(mtm) };
        // The page chose the rect: keep it inside what the view shows, so a grab
        // can't ask WebKit to render a huge offscreen area.
        let shown = page.frame().size;
        let [x, y, w, h] = rect;
        let (x0, y0) = (x.clamp(0.0, shown.width), y.clamp(0.0, shown.height));
        let (x1, y1) = ((x + w).clamp(0.0, shown.width), (y + h).clamp(0.0, shown.height));
        let (x, y, w, h) = (x0, y0, x1 - x0, y1 - y0);
        if w < 1.0 || h < 1.0 {
            return done(None);
        }
        // SAFETY: plain setter on a fresh configuration, main thread.
        unsafe { config.setRect(CGRect { origin: CGPoint { x, y }, size: CGSize { width: w, height: h } }) };
        let done = RefCell::new(Some(done));
        let handler = RcBlock::new(move |image: *mut NSImage, _err: *mut NSError| {
            let Some(done) = done.borrow_mut().take() else { return };
            // SAFETY: WebKit hands a live NSImage (or null) to this block.
            let png = unsafe { image.as_ref() }
                .and_then(|i| i.TIFFRepresentation())
                .and_then(|tiff| NSBitmapImageRep::imageRepWithData(&tiff))
                .and_then(|rep| unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new()) });
            done(png.map(|data| data.to_vec()));
        });
        // SAFETY: the page is alive (held in PAGES); the handler owns what it needs.
        unsafe { page.takeSnapshotWithConfiguration_completionHandler(Some(&config), &handler) };
    }

    /// Asks the person, in a sheet on the page's window, whether agents may
    /// read and drive `site` in block `id`; `answer` gets their choice.
    pub fn ask(id: &str, site: &str, answer: impl FnOnce(bool) + 'static) {
        let window = BOXES.with_borrow(|v| v.iter().find(|(_, b)| b == id).and_then(|(view, _)| view.window()));
        let (Some(window), Some(mtm)) = (window, MainThreadMarker::new()) else { return answer(false) };
        let alert = NSAlert::new(mtm);
        alert.setMessageText(&NSString::from_str("Let agents use this page?"));
        alert.setInformativeText(&NSString::from_str(&format!(
            "An agent wants to read and control {site}. Agents can then click, type and read on it as you, signed in, until Herdr quits."
        )));
        alert.addButtonWithTitle(&NSString::from_str("Allow"));
        alert.addButtonWithTitle(&NSString::from_str("Don't Allow"));
        let answer = RefCell::new(Some(answer));
        let handler = RcBlock::new(move |response: NSModalResponse| {
            if let Some(answer) = answer.borrow_mut().take() {
                answer(response == NSAlertFirstButtonReturn);
            }
        });
        alert.beginSheetModalForWindow_completionHandler(&window, Some(&handler));
    }

    pub fn set_layout(name: &str) {
        LAYOUT.with_borrow_mut(|l| if l != name { *l = name.to_owned() });
    }

    /// Tells mods which page has the keyboard (`shared("web")[id].focused`).
    pub fn focused(id: Option<&str>) {
        let changed = FOCUSED.with_borrow_mut(|f| {
            if f.as_deref() == id {
                return None;
            }
            Some(std::mem::replace(f, id.map(str::to_owned)))
        });
        if let Some(old) = changed {
            if let Some(old) = old {
                super::publish(&old, "focused", serde_json::json!(false));
            }
            if let Some(id) = id {
                super::publish(id, "focused", serde_json::json!(true));
            }
        }
    }

    pub struct Container {
        view: Retained<NSView>,
    }

    impl Container {
        /// A box inside GPUI's view, where `b` is.
        pub fn new(window: &Window, b: Bounds<Pixels>, id: &str) -> Option<Self> {
            let mtm = MainThreadMarker::new()?;
            let RawWindowHandle::AppKit(h) = HasWindowHandle::window_handle(window).ok()?.as_raw() else {
                return None;
            };
            // SAFETY: GPUI hands out its live content NSView; we only borrow it here.
            let parent: &NSView = unsafe { h.ns_view.cast::<NSView>().as_ref() };
            let view = NSView::initWithFrame(mtm.alloc(), frame(parent, b));
            view.setAutoresizesSubviews(true);
            parent.addSubview(&view);
            GPUI.with_borrow_mut(|g| *g = Some(parent.retain()));
            BOXES.with_borrow_mut(|v| v.push((view.clone(), id.to_owned())));
            install_monitor();
            Some(Container { view })
        }

        /// Moves the box; the page (and a docked inspector) resize with it.
        pub fn place(&self, b: Bounds<Pixels>) {
            // SAFETY: reading the superview of a view we own, on the main thread.
            if let Some(parent) = unsafe { self.view.superview() } {
                self.view.setFrame(frame(&parent, b));
            }
        }

        pub fn set_visible(&self, show: bool) {
            self.view.setHidden(!show);
            // A hidden page can't keep the keyboard: give it back to Herdr.
            if !show && has_keyboard(&self.view) {
                give_back();
            }
        }

        /// The page follows the box's size from now on.
        pub fn fill_with(&self, page: &wry::WebView, id: &str) {
            let wk: Retained<WKWebView> = Retained::into_super(page.webview());
            PAGES.with_borrow_mut(|p| p.insert(id.to_owned(), wk));
            page.webview().setAutoresizingMask(
                NSAutoresizingMaskOptions::ViewWidthSizable
                    | NSAutoresizingMaskOptions::ViewHeightSizable,
            );
        }
    }

    impl Drop for Container {
        fn drop(&mut self) {
            BOXES.with_borrow_mut(|v| v.retain(|(b, _)| !std::ptr::eq(&**b, &*self.view)));
            self.view.removeFromSuperview();
        }
    }

    /// Whether the window's keyboard focus is inside `boxed` (the page, or its inspector).
    fn has_keyboard(boxed: &NSView) -> bool {
        let Some(window) = boxed.window() else { return false };
        let Some(responder) = window.firstResponder() else { return false };
        let any: &AnyObject = &responder;
        any.downcast_ref::<NSView>().is_some_and(|v| v.isDescendantOf(boxed))
    }

    /// The block whose page has the keyboard now, if any.
    fn page_with_keyboard() -> Option<String> {
        BOXES.with_borrow(|v| v.iter().find(|(b, _)| has_keyboard(b)).map(|(_, id)| id.clone()))
    }

    /// Hands the keyboard back to GPUI's view.
    pub fn give_back() {
        GPUI.with_borrow(|g| {
            if let Some(view) = g
                && let Some(window) = view.window()
            {
                window.makeFirstResponder(Some(view));
            }
        });
        focused(None);
    }

    /// Whether a window point (AppKit's, bottom-left) falls in a box; its block id.
    fn page_at(event: &NSEvent) -> Option<String> {
        let at = event.locationInWindow();
        BOXES.with_borrow(|v| {
            v.iter()
                .find(|(b, _)| {
                    if b.isHidden() {
                        return false;
                    }
                    let p = b.convertPoint_fromView(at, None);
                    let r = b.bounds();
                    p.x >= r.origin.x && p.y >= r.origin.y && p.x < r.origin.x + r.size.width && p.y < r.origin.y + r.size.height
                })
                .map(|(_, id)| id.clone())
        })
    }

    fn install_monitor() {
        if MONITOR.replace(true) {
            return;
        }
        let handler = RcBlock::new(|event: NonNull<NSEvent>| -> *mut NSEvent {
            // SAFETY: AppKit passes a live event, on the main thread.
            let ev = unsafe { event.as_ref() };
            match ev.r#type() {
                NSEventType::KeyDown => {
                    if page_with_keyboard().is_some() {
                        // SAFETY: a live key-down NSEvent.
                        let stroke = unsafe { gpui_macos::keystroke_for_event(event.as_ptr().cast()) }.unparse();
                        let layout = LAYOUT.with_borrow(|l| l.clone());
                        let owned = crate::keys::herdr_owns(&stroke, &layout);
                        if owned {
                            // GPUI's view becomes first responder, then AppKit delivers
                            // this same event there: Herdr runs the key.
                            give_back();
                        }
                    }
                }
                // Clicks decide which page (if any) has the keyboard, for mods to show.
                NSEventType::LeftMouseDown | NSEventType::RightMouseDown => {
                    let page = page_at(ev);
                    focused(page.as_deref());
                    // A web pane's click focuses the pane, as a terminal's does.
                    if let Some(id) = page {
                        crate::mods::request(crate::mods::Request::WebClicked(id));
                    }
                }
                _ => {}
            }
            event.as_ptr()
        });
        // SAFETY: the handler only touches main-thread state and returns the event.
        let token = unsafe {
            NSEvent::addLocalMonitorForEventsMatchingMask_handler(
                NSEventMask::KeyDown | NSEventMask::LeftMouseDown | NSEventMask::RightMouseDown,
                &handler,
            )
        };
        // The monitor lives as long as the app.
        std::mem::forget(token);
    }

    /// GPUI's coordinates start top-left; AppKit's start bottom-left unless flipped.
    fn frame(parent: &NSView, b: Bounds<Pixels>) -> CGRect {
        let (x, y) = (f32::from(b.origin.x) as f64, f32::from(b.origin.y) as f64);
        let (w, h) = (f32::from(b.size.width).max(1.0) as f64, f32::from(b.size.height).max(1.0) as f64);
        let y = if parent.isFlipped() { y } else { parent.frame().size.height - y - h };
        CGRect { origin: CGPoint { x, y }, size: CGSize { width: w, height: h } }
    }

    /// wry builds its page as a child of whatever window handle it's given: this box.
    impl HasWindowHandle for Container {
        fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
            let ptr = NonNull::from(&*self.view).cast();
            // SAFETY: the NSView outlives the borrow (we hold a strong reference).
            Ok(unsafe { WindowHandle::borrow_raw(RawWindowHandle::AppKit(AppKitWindowHandle::new(ptr))) })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigate_opens_only_valid_web_addresses() {
        assert_eq!(address("github.com").unwrap(), "https://github.com/");
        assert_eq!(address("  https://herdr.dev/a b ").unwrap(), "https://herdr.dev/a%20b");
        assert_eq!(address("HTTP://example.com").unwrap(), "http://example.com/");
        assert_eq!(address("localhost:3000/x").unwrap(), "https://localhost:3000/x");
        for bad in ["javascript:alert(1)", "data:text/html,<b>hi</b>", "file:///no/such/file.html", "about:blank", "hello world", "", "https://", "ftp://x.org"] {
            assert!(address(bad).is_err(), "{bad:?} should be refused");
        }
    }

    #[test]
    fn local_pages_read_only_their_own_folder() {
        let dir = std::env::temp_dir().join(format!("herdr-web-local-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("site")).unwrap();
        std::fs::write(dir.join("site/index.html"), "<p>hi</p>").unwrap();
        std::fs::write(dir.join("site/style.css"), "p{}").unwrap();
        std::fs::write(dir.join("secret.txt"), "no").unwrap();
        let page = address(dir.join("site/index.html").to_str().unwrap()).unwrap();
        assert!(page.starts_with("herdr-file://localhost/"), "{page}");
        assert_eq!(address(&format!("file://{}", dir.join("site/index.html").display())).unwrap(), page);
        // Typed herdr-file: addresses are checked like paths: a folder isn't a page.
        assert_eq!(address(&page).unwrap(), page);
        assert!(address(&page.replace("index.html", "")).is_err());
        assert!(address("herdr-file://localhost/no/such/file.html").is_err());
        let get = |uri: &str, origin: Option<&str>| {
            let mut r = wry::http::Request::builder().uri(uri);
            if let Some(o) = origin {
                r = r.header("Origin", o);
            }
            r.body(Vec::new()).unwrap()
        };
        allow_folder("b", &page);
        let css = page.replace("index.html", "style.css");
        let r = serve_local("b", &get(&css, None));
        assert_eq!(r.status(), 200);
        assert_eq!(r.headers()["Content-Type"], "text/css; charset=utf-8");
        assert_eq!(r.headers()["Content-Security-Policy"], LOCAL_CSP);
        assert_eq!(serve_local("b", &get(&css, Some("herdr-file://localhost"))).status(), 200);
        assert_eq!(serve_local("b", &get(&css, Some("https://evil.example"))).status(), 403);
        assert_eq!(serve_local("b", &get(&page.replace("site/index.html", "secret.txt"), None)).status(), 403);
        assert_eq!(serve_local("b", &get(&page.replace("index.html", "../secret.txt"), None)).status(), 403);
        assert_eq!(serve_local("other", &get(&css, None)).status(), 403);
        // The page itself can't leave for the web; Herdr's web.navigate can.
        assert!(may_navigate("b", &css));
        assert!(!may_navigate("b", "https://evil.example/?d=secret"));
        assert!(may_navigate("other", "https://herdr.dev/"));
        // Navigating away from the local file takes the folder back.
        allow_folder("b", "https://herdr.dev/");
        assert!(may_navigate("b", "https://herdr.dev/"));
        assert_eq!(serve_local("b", &get(&css, None)).status(), 403);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn grabs_lose_control_characters_and_stay_one_line() {
        let g = json!({
            "page": { "url": "https://x.dev/", "title": "a\nb" },
            "tag": "p", "text": "Hello\u{1b}[31m red\r\nnext\u{202E}line", "selector": "#p",
            "html": "<p>\u{1b}]0;title\u{7}x\ny</p>", "rect": { "x": 0, "y": 0, "width": 10, "height": 10 },
        });
        let c = clamp_grab(&g).unwrap();
        assert_eq!(c["text"], "Hello[31m red nextline");
        assert_eq!(c["page"]["title"], "a b");
        assert_eq!(c["html"], "<p>]0;titlex\ny</p>");
        assert!(clamp_grab(&json!({ "page": { "url": "javascript:x" }, "rect": {} })).is_none());
    }

    #[test]
    fn a_design_token_works_once() {
        NONCES.with_borrow_mut(|n| n.insert("b".into(), "tok".into()));
        let msg = |t: &str| json!({ "nonce": t }).as_object().unwrap().clone();
        assert!(!take_nonce("b", &msg("wrong")));
        assert!(take_nonce("b", &msg("tok")));
        assert!(!take_nonce("b", &msg("tok")));
        assert_ne!(new_nonce(), new_nonce());
    }
}
