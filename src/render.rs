//! Card renderer — full HTML/CSS → RGBA via the pure-Rust CPU pipeline
//! (blitz-dom + stylo + taffy + parley + vello_cpu). No webview, no GPU.
//!
//! The device exposes no usable font discovery to a static-musl binary, so we
//! build ONE `FontContext` with embedded fonts + CSS-generic mapping (proven in
//! spike B) and clone it per card. `FontContext` is cheap to clone.
//!
//! Embedded fonts: Noto Sans (regular/bold/italic/bolditalic) + Noto Mono, plus
//! Noto Symbols2 + Noto Emoji (monochrome) as glyph fallbacks so symbol/emoji
//! glyphs render instead of tofu.
//!
//! Card media (`<img src>` and `@font-face` files) is served from the collection's
//! media folder via a local `NetProvider`; after the first layout we drain the
//! fetched resources and re-resolve until images settle (one-shot render, no event
//! loop).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyrender::render_to_buffer;
use anyrender_vello_cpu::VelloCpuImageRenderer;
use blitz_dom::net::Resource;
use blitz_dom::DocumentConfig;
use blitz_html::HtmlDocument;
use blitz_paint::paint_scene;
use blitz_traits::net::{BoxedHandler, Bytes, NetProvider, Request, SharedCallback};
use blitz_traits::shell::{ColorScheme, Viewport};
use parley::fontique::{Blob, FontInfoOverride, GenericFamily, Script};
use parley::FontContext;

const F_SANS_REG: &[u8] = include_bytes!("../assets/fonts/NotoSans-Regular.ttf");
const F_SANS_BOLD: &[u8] = include_bytes!("../assets/fonts/NotoSans-Bold.ttf");
const F_SANS_IT: &[u8] = include_bytes!("../assets/fonts/NotoSans-Italic.ttf");
const F_SANS_BIT: &[u8] = include_bytes!("../assets/fonts/NotoSans-BoldItalic.ttf");
const F_MONO: &[u8] = include_bytes!("../assets/fonts/NotoMono-Regular.ttf");
const F_SYMBOLS: &[u8] = include_bytes!("../assets/fonts/NotoSansSymbols2-Regular.ttf");
const F_EMOJI: &[u8] = include_bytes!("../assets/fonts/NotoEmoji-Regular.ttf");
// Nerd Font symbols (powerline / devicons / font-awesome, all in the Private Use
// Area) — cards that use Nerd Font icons would otherwise show tofu boxes.
const F_NERD: &[u8] = include_bytes!("../assets/fonts/SymbolsNerdFontMono-Regular.ttf");
// XITS Math — broad math-symbol coverage (arrows like ⇒ U+21D2, operators, greek)
// that cards use as literal unicode in prose. Without it those show tofu boxes.
const F_XITS: &[u8] = include_bytes!("../assets/fonts/rex-xits.otf");

pub struct Renderer {
    font_ctx: FontContext,
    media_dir: Option<PathBuf>,
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new()
    }
}

impl Renderer {
    pub fn new() -> Self {
        Self {
            font_ctx: build_font_ctx(),
            media_dir: None,
        }
    }

    /// Serve card `<img>`/`@font-face` resources from this collection media folder.
    pub fn with_media_dir(mut self, dir: PathBuf) -> Self {
        self.media_dir = Some(dir);
        self
    }

    /// Build a laid-out document at `width`×`height` with media resolved.
    fn build_document(&self, html: &str, width: u32, height: u32) -> HtmlDocument {
        let viewport = Viewport::new(width, height, 1.0, ColorScheme::Light);
        let mut doc_config = DocumentConfig {
            viewport: Some(viewport),
            font_ctx: Some(self.font_ctx.clone()),
            ..Default::default()
        };

        // Wire local media resolution if we know the media folder.
        let queue: Arc<Mutex<Vec<Resource>>> = Arc::new(Mutex::new(Vec::new()));
        if let Some(dir) = &self.media_dir {
            let q = queue.clone();
            let callback: SharedCallback<Resource> =
                Arc::new(move |_id: usize, res: Result<Resource, Option<String>>| {
                    if let Ok(r) = res {
                        q.lock().unwrap().push(r);
                    }
                });
            doc_config.net_provider = Some(Arc::new(LocalMediaProvider { callback }));
            // Relative img/font urls resolve against this (trailing slash matters).
            doc_config.base_url = Some(format!("file://{}/", dir.display()));
        }

        let mut document = HtmlDocument::from_html(html, doc_config);
        document.resolve(0.0);

        // Drain media fetched during layout, apply, re-resolve until settled.
        if self.media_dir.is_some() {
            for _ in 0..8 {
                let pending: Vec<Resource> = std::mem::take(&mut *queue.lock().unwrap());
                if pending.is_empty() {
                    break;
                }
                for r in pending {
                    document.load_resource(r);
                }
                document.resolve(0.0);
            }
        }
        document
    }

    /// Render `html` to a tightly-packed RGBA8888 buffer of `width * height * 4`
    /// bytes (row-major). Background comes from the HTML body's CSS.
    pub fn render_rgba(&self, html: &str, width: u32, height: u32) -> Vec<u8> {
        let document = self.build_document(html, width, height);
        render_to_buffer::<VelloCpuImageRenderer, _>(
            |scene| paint_scene(scene, document.as_ref(), 1.0, width, height),
            width,
            height,
        )
    }

    /// Build a LIVE card view for the scrollable/zoomable card region: the document
    /// stays resident so scrolling is a repaint (no re-layout) and zoom is a real
    /// reflow (text re-wraps to the card width at the new scale).
    pub fn build_card_view(&self, html: &str, width: u32, height: u32) -> CardView {
        self.build_card_view_at(html, width, height, 0)
    }

    /// `build_card_view` laid out directly at `ZOOM_STEPS[zoom_idx]` — the
    /// persisted zoom preference, so every card opens at the level you last
    /// pinched to instead of snapping back to 1.0×.
    pub fn build_card_view_at(&self, html: &str, width: u32, height: u32, zoom_idx: usize) -> CardView {
        let doc = self.build_document(html, width, height);
        let mut view = CardView {
            doc,
            width,
            height,
            zoom_idx: 0,
        };
        view.refresh_content_size();
        let zoom_idx = zoom_idx.min(ZOOM_STEPS.len() - 1);
        if zoom_idx != 0 {
            view.apply_zoom(zoom_idx, 0.0);
        }
        view
    }
}

/// Stepped zoom levels — e-ink can't animate a continuous pinch, so a completed
/// pinch snaps one step in/out and re-renders crisp.
pub const ZOOM_STEPS: [f32; 4] = [1.0, 1.25, 1.5, 2.0];

/// A live, scrollable, zoomable render of one card side.
///
/// Scroll offset lives INSIDE the blitz document (`viewport_scroll`, CSS px);
/// painting translates by it, so a scroll step needs no re-layout. Zoom uses the
/// viewport's real zoom factor → full reflow at the new scale.
pub struct CardView {
    doc: HtmlDocument,
    width: u32,
    height: u32,
    zoom_idx: usize,
}

impl CardView {
    pub fn zoom(&self) -> f32 {
        ZOOM_STEPS[self.zoom_idx]
    }

    /// Index into `ZOOM_STEPS` of the current zoom level.
    pub fn zoom_idx(&self) -> usize {
        self.zoom_idx
    }

    /// Content height in PHYSICAL px at the current zoom.
    pub fn content_h(&self) -> f64 {
        let scale = self.doc.viewport().scale() as f64;
        let css_h = self.doc.root_element().final_layout.size.height as f64;
        css_h * scale
    }

    /// Current scroll offset in PHYSICAL px.
    pub fn scroll_phys(&self) -> f64 {
        let scale = self.doc.viewport().scale() as f64;
        self.doc.viewport_scroll().y * scale
    }

    /// Current scroll offset in CSS px (`scroll_phys` = this × zoom) — the space
    /// whiteboard strokes are anchored in.
    pub fn scroll_css(&self) -> f64 {
        self.doc.viewport_scroll().y
    }

    /// Maximum scroll offset in physical px (0 when the card fits the window).
    pub fn max_scroll(&self) -> f64 {
        (self.content_h() - self.height as f64).max(0.0)
    }

    fn refresh_content_size(&mut self) {
        // Nothing cached currently — content_h reads the live layout — but keep the
        // hook so a future cached-tall-buffer optimization has one place to land.
    }

    /// Scroll by `dy` physical px (positive = further down the card). Returns true
    /// if the offset actually changed (clamped at the ends).
    pub fn scroll_by(&mut self, dy: f64) -> bool {
        let scale = self.doc.viewport().scale() as f64;
        // scroll_viewport_by computes `new = current - y` and clamps to content.
        self.doc.scroll_viewport_by_has_changed(0.0, -(dy / scale))
    }

    /// Step zoom in (`+1`) or out (`-1`), preserving the reading position as a
    /// fraction of content height. Returns true if the zoom level changed.
    pub fn set_zoom_step(&mut self, dir: i32) -> bool {
        let new_idx = (self.zoom_idx as i32 + dir).clamp(0, ZOOM_STEPS.len() as i32 - 1) as usize;
        if new_idx == self.zoom_idx {
            return false;
        }
        let frac = if self.content_h() > 0.0 {
            self.scroll_phys() / self.content_h()
        } else {
            0.0
        };
        self.apply_zoom(new_idx, frac);
        true
    }

    /// Reflow at `ZOOM_STEPS[new_idx]` and scroll to `frac` of the new content
    /// height (0.0 = top). Shared by the pinch step and the persisted-zoom build.
    fn apply_zoom(&mut self, new_idx: usize, frac: f64) {
        self.zoom_idx = new_idx;
        let mut vp = Viewport::new(self.width, self.height, 1.0, ColorScheme::Light);
        vp.set_zoom(ZOOM_STEPS[new_idx]);
        self.doc.set_viewport(vp);
        self.doc.resolve(0.0);
        // Re-anchor: scroll to the same content fraction at the new size, using the
        // clamped scroll_viewport_by (zero it, then move down by the target).
        let scale = self.doc.viewport().scale() as f64;
        let target_css = (frac * self.content_h()) / scale;
        self.doc.scroll_viewport_by(0.0, 1.0e9); // clamp to top
        self.doc.scroll_viewport_by(0.0, -target_css); // clamped downward move
    }

    /// Paint the current window (width×height at the current scroll/zoom) to RGBA.
    pub fn paint_window(&self) -> Vec<u8> {
        let scale = self.doc.viewport().scale() as f64;
        render_to_buffer::<VelloCpuImageRenderer, _>(
            |scene| paint_scene(scene, self.doc.as_ref(), scale, self.width, self.height),
            self.width,
            self.height,
        )
    }
}

/// Family names decks ask for that we don't ship, mapped onto the font we do.
///
/// parley drops a named family it can't find (`arial` → nothing), and Anki's
/// stock notetype CSS is `.card { font-family: arial; }`. Registering the Noto
/// faces again under these names (same bytes — `Blob` is shared, not copied)
/// makes such a card resolve to the real family, with bold/italic matching
/// intact, instead of falling through to the glyph-fallback chain.
const SANS_ALIASES: &[&str] = &[
    "arial", "helvetica", "helvetica neue", "verdana", "tahoma", "segoe ui", "calibri",
    "trebuchet ms", "roboto", "open sans", "lato", "ubuntu", "dejavu sans", "liberation sans",
    "arial unicode ms", "lucida grande", "lucida sans unicode", "gill sans", "optima",
    "avenir", "avenir next", "futura", "inter", "source sans pro", "fira sans", "nunito",
    "montserrat", "raleway", "pt sans", "droid sans", "san francisco", "sf pro text",
    "sf pro display", "-apple-system", "blinkmacsystemfont",
    // Serif names map here too: we ship no serif, and the `serif` generic already
    // resolves to Noto Sans. Better the right glyphs in sans than fallback soup.
    "times", "times new roman", "georgia", "cambria", "garamond", "palatino",
    "palatino linotype", "book antiqua", "baskerville", "dejavu serif", "liberation serif",
    "noto serif", "pt serif", "merriweather", "droid serif", "charter", "constantia",
];
const MONO_ALIASES: &[&str] = &[
    "courier", "courier new", "consolas", "menlo", "monaco", "lucida console", "andale mono",
    "dejavu sans mono", "liberation mono", "source code pro", "fira code", "fira mono",
    "jetbrains mono", "ubuntu mono", "roboto mono", "inconsolata", "sf mono", "cascadia code",
    "cascadia mono", "droid sans mono", "noto sans mono", "hack", "iosevka",
];

/// Embedded fonts registered, CSS generics mapped to Noto Sans/Mono, common
/// deck font names aliased onto them, and Noto Sans + the symbol fonts installed
/// as the glyph-fallback chain so any deck's `font-family` resolves and stray
/// symbol/emoji/Greek glyphs render instead of tofu.
fn build_font_ctx() -> FontContext {
    let mut fcx = FontContext::new();

    // One shared Blob per face; aliases re-register the same bytes under another
    // family name without copying them.
    let sans_blobs: Vec<Blob<u8>> = [F_SANS_REG, F_SANS_BOLD, F_SANS_IT, F_SANS_BIT]
        .iter()
        .map(|b| Blob::from(b.to_vec()))
        .collect();
    let mono_blob = Blob::from(F_MONO.to_vec());

    let mut sans_ids = Vec::new();
    for blob in &sans_blobs {
        for (fam, _) in fcx.collection.register_fonts(blob.clone(), None) {
            if !sans_ids.contains(&fam) {
                sans_ids.push(fam);
            }
        }
    }
    let mut mono_ids = Vec::new();
    for (fam, _) in fcx.collection.register_fonts(mono_blob.clone(), None) {
        mono_ids.push(fam);
    }
    for alias in SANS_ALIASES {
        let over = FontInfoOverride {
            family_name: Some(alias),
            ..Default::default()
        };
        for blob in &sans_blobs {
            fcx.collection.register_fonts(blob.clone(), Some(over));
        }
    }
    for alias in MONO_ALIASES {
        let over = FontInfoOverride {
            family_name: Some(alias),
            ..Default::default()
        };
        fcx.collection.register_fonts(mono_blob.clone(), Some(over));
    }

    let mut symbol_ids = Vec::new();
    let mut emoji_ids = Vec::new();
    let mut math_ids = Vec::new();
    for (i, bytes) in [F_SYMBOLS, F_EMOJI, F_NERD, F_XITS].iter().enumerate() {
        for (fam, _) in fcx
            .collection
            .register_fonts(Blob::from(bytes.to_vec()), None)
        {
            symbol_ids.push(fam);
            match i {
                1 => emoji_ids.push(fam),
                3 => math_ids.push(fam),
                _ => {}
            }
        }
    }

    for g in [
        GenericFamily::SansSerif,
        GenericFamily::Serif,
        GenericFamily::SystemUi,
        GenericFamily::UiSansSerif,
        GenericFamily::UiSerif,
        GenericFamily::UiRounded,
        GenericFamily::Cursive,
        GenericFamily::Fantasy,
    ] {
        fcx.collection
            .set_generic_families(g, sans_ids.iter().copied());
    }
    for g in [GenericFamily::Monospace, GenericFamily::UiMonospace] {
        fcx.collection
            .set_generic_families(g, mono_ids.iter().copied());
    }
    fcx.collection
        .set_generic_families(GenericFamily::Emoji, emoji_ids.iter().copied());
    fcx.collection
        .set_generic_families(GenericFamily::Math, math_ids.iter().copied());

    // Glyph-level fallback: when a matched font lacks a glyph, parley queries
    // fontique for fallback families by the cluster's *script*. With no system
    // fonts, the default (named) fallbacks resolve to nothing, so we install our
    // own chain on every script a card is likely to carry.
    //
    // Noto Sans leads the chain. When a deck names a family we neither ship nor
    // alias, parley resolves NO family and draws every glyph from this chain —
    // and the old chain started with Symbols2, whose Latin letters are Noto Sans
    // outlines (so prose looked right) but whose digits are 1.1 em wide
    // ("spaced-out numbers") and which has no Greek at all (β simply vanished,
    // since Greek had no fallback registered). Leading with Noto Sans makes an
    // unknown family render exactly like `sans-serif`; the symbol fonts still
    // catch whatever Noto Sans lacks. `Zzzz` = Unknown script, which is what
    // Private-Use-Area codepoints (Nerd Font icons) resolve to.
    let chain: Vec<_> = sans_ids
        .iter()
        .chain(symbol_ids.iter())
        .copied()
        .collect();
    for tag in [
        b"Latn", b"Grek", b"Cyrl", b"Zyyy", b"Zinh", b"Zsye", b"Zsym", b"Zmth", b"Zzzz",
    ] {
        fcx.collection
            .append_fallbacks(Script(*tag), chain.iter().copied());
    }

    fcx
}

/// Serves blitz resource requests from the local filesystem (the collection media
/// folder, via the document base_url) and from `data:` URIs. Calls the provided
/// handler synchronously so resources are queued during `resolve()`.
struct LocalMediaProvider {
    callback: SharedCallback<Resource>,
}

impl NetProvider<Resource> for LocalMediaProvider {
    fn fetch(&self, doc_id: usize, request: Request, handler: BoxedHandler<Resource>) {
        let url = request.url;
        let bytes: Option<Vec<u8>> = match url.scheme() {
            "file" => url.to_file_path().ok().and_then(|p| std::fs::read(p).ok()),
            "data" => decode_data_uri(url.as_str()),
            _ => None,
        };
        if let Some(b) = bytes {
            handler.bytes(doc_id, Bytes::from(b), self.callback.clone());
        }
        // Missing/unsupported → drop handler; the resource simply won't appear.
    }
}

/// Minimal `data:[<mime>][;base64],<data>` decoder (base64 payloads only).
fn decode_data_uri(s: &str) -> Option<Vec<u8>> {
    let comma = s.find(',')?;
    let (meta, data) = s.split_at(comma);
    let data = &data[1..];
    if meta.contains(";base64") {
        base64_decode(data)
    } else {
        Some(data.as_bytes().to_vec())
    }
}

/// Standard-alphabet base64 decode (no external dep; tolerant of whitespace).
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a' + 26) as u32),
            b'0'..=b'9' => Some((c - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut acc = 0u32;
    let mut nbits = 0u32;
    for &c in s.as_bytes() {
        if c == b'=' || c.is_ascii_whitespace() {
            continue;
        }
        let v = val(c)?;
        acc = (acc << 6) | v;
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out.push((acc >> nbits) as u8);
        }
    }
    Some(out)
}
