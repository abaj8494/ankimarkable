//! Whiteboard ink layer — an AnkiDroid-style pen scratch surface over the card.
//!
//! Model: we keep the composited review frame as an opaque **base** (RGBA, no
//! ink) plus a single-channel **ink** coverage buffer (0=clear, 255=black). The
//! on-screen QTFB framebuffer is always `base` with `ink` composited over it
//! (ink is black, so `out = base * (255-cov) / 255`). Strokes are recorded for
//! undo; the ink buffer is authoritative and can be rebuilt by replaying them.
//!
//! Live inking writes only the moving segment into `ink`, recomputes the affected
//! framebuffer pixels, and issues a fast (DU) partial refresh over the dirty rect
//! — so writing tracks the pen. Phase changes (Show Answer) replace `base` and
//! recomposite with the ink kept on top; advancing to the next card clears ink.
//!
//! Strokes are stored in **content CSS coords** (≡ panel px at zoom 1.0): capture
//! divides panel coords by the current zoom and adds the CSS scroll, replay does
//! the inverse. That lets ink survive scroll AND zoom changes — it re-anchors to
//! the CSS geometry it was written over. Accepted tradeoffs: zoom is a real
//! reflow, so ink anchors to CSS *positions*, not to re-wrapped words; and a
//! stroke drawn at 2× renders proportionally thin at 1×.
//!
//! Ink is confined to the card region (`ui::CARD_Y .. CARD_Y+CARD_H`); pen samples
//! over the top/bottom chrome are ignored so writing never spills onto the bars.

use crate::pen::PenSample;
use crate::qtfb::{Qtfb, REFRESH_MODE_ANIMATE, REFRESH_MODE_CONTENT, REFRESH_MODE_FAST};
use crate::ui::{CARD_H, CARD_Y, HEIGHT, WIDTH};

/// Eraser brush radius (px) — much larger than a pen stroke so the eraser end
/// clears meaningfully, matching the scratchpad feel.
const ERASE_RADIUS: i32 = 26;

struct Stroke {
    erase: bool,
    // (x, y, width) in content CSS coords — panel coords divided by the zoom at
    // capture, y offset by the CSS scroll. rebuild_ink maps back through the
    // CURRENT scroll/zoom, so ink tracks the card through both.
    pts: Vec<(i32, i32, u8)>,
}

pub struct Whiteboard {
    base: Vec<u8>, // WIDTH*HEIGHT*4 RGBA — composited review/message, no ink
    ink: Vec<u8>,  // WIDTH*HEIGHT coverage
    strokes: Vec<Stroke>,
    active: Option<Stroke>,
    last: Option<(i32, i32)>,
    /// Card scroll offset (CSS px) the ink is currently rebuilt for.
    scroll: f32,
    /// Card zoom the ink is currently rebuilt for (content CSS × zoom = panel px).
    zoom: f32,
    // Accumulated dirty rect (x0,y0,x1,y1) pending a coalesced refresh — samples
    // ink into the framebuffer but the e-ink refresh is batched (one per input
    // drain, not one per sample), which is what keeps inking smooth.
    dirty: Option<(i32, i32, i32, i32)>,
    // Region inked with the fast (DU, grayscale) waveform since the last color
    // settle. On the rMPP colour panel a DU refresh drops colour; once writing
    // pauses we re-refresh this region in colour (GC16) so the card's colour comes
    // back with the ink kept crisp.
    settle: Option<(i32, i32, i32, i32)>,
    pub eraser: bool,
}

fn in_card(y: i32) -> bool {
    y >= CARD_Y as i32 && y < (CARD_Y + CARD_H) as i32
}

impl Whiteboard {
    pub fn new() -> Self {
        Self {
            base: vec![255u8; WIDTH * HEIGHT * 4],
            ink: vec![0u8; WIDTH * HEIGHT],
            strokes: Vec::new(),
            active: None,
            last: None,
            scroll: 0.0,
            zoom: 1.0,
            dirty: None,
            settle: None,
            eraser: false,
        }
    }

    pub fn has_ink(&self) -> bool {
        !self.strokes.is_empty() || self.active.is_some()
    }

    /// Replace the base frame (e.g. after composing the answer side). Ink is kept.
    pub fn set_base(&mut self, frame: Vec<u8>) {
        debug_assert_eq!(frame.len(), self.base.len());
        self.base = frame;
    }

    /// Mutable access to the base frame so scroll steps can patch just the card
    /// band without a full recompose (see `ui::patch_card_band`).
    pub fn base_mut(&mut self) -> &mut [u8] {
        &mut self.base
    }

    /// The card view scrolled and/or zoomed: re-anchor the ink to the new offset
    /// and scale (strokes are stored in content CSS coords, so this is a replay
    /// through the new transform).
    pub fn set_view(&mut self, scroll_css: f32, zoom: f32) {
        // Zoom comes from render::ZOOM_STEPS; anything non-positive would make the
        // capture transform divide by zero (or mirror the ink).
        debug_assert!(zoom > 0.0, "zoom must be positive (got {zoom})");
        if (scroll_css - self.scroll).abs() < 0.01 && (zoom - self.zoom).abs() < 0.001 {
            return;
        }
        self.scroll = scroll_css;
        self.zoom = zoom;
        // A stroke can't sanely continue across a scroll/zoom change.
        self.end_stroke();
        self.rebuild_ink();
    }

    /// Drop all ink + strokes (used when advancing to the next card).
    pub fn clear_ink(&mut self) {
        self.strokes.clear();
        self.active = None;
        self.last = None;
        self.dirty = None;
        self.settle = None;
        for b in self.ink.iter_mut() {
            *b = 0;
        }
    }

    pub fn toggle_eraser(&mut self) {
        self.eraser = !self.eraser;
    }

    /// Force-close any in-progress stroke without needing a pen-up sample. Recovery
    /// path for a dropped BTN_TOUCH release: an `active` stroke that never ends keeps
    /// `is_inking()` true, which latches palm-rejection on and swallows every finger
    /// tap. Identical bookkeeping to the `s.up` branch of `ink_sample`.
    pub fn end_stroke(&mut self) {
        if let Some(st) = self.active.take() {
            if !st.pts.is_empty() {
                self.strokes.push(st);
            }
        }
        self.last = None;
    }

    /// Drop a pending colour settle WITHOUT refreshing. In black-and-white mode there
    /// is no colour to restore, so the fast-waveform ink stands as-is; this just
    /// clears `needs_settle()` so the loop stops queuing a redundant GC16 flash.
    pub fn discard_settle(&mut self) {
        self.settle = None;
    }

    /// Push `base` composited with `ink` into the framebuffer and do a clean full
    /// (GC16) refresh — used after any non-inking screen change.
    pub fn blit_full(&mut self, qtfb: &mut Qtfb) {
        let shm: &mut [u8] = &mut *qtfb.shm;
        for idx in 0..(WIDTH * HEIGHT) {
            let bi = idx * 4;
            let di = idx * 3;
            let cov = self.ink[idx] as u32;
            if cov == 0 {
                shm[di] = self.base[bi];
                shm[di + 1] = self.base[bi + 1];
                shm[di + 2] = self.base[bi + 2];
            } else {
                let ia = 255 - cov;
                for k in 0..3 {
                    shm[di + k] = ((self.base[bi + k] as u32 * ia) / 255) as u8;
                }
            }
        }
        let _ = qtfb.set_refresh_mode(REFRESH_MODE_CONTENT);
        let _ = qtfb.update_full();
        // The full GC16 colour refresh supersedes any pending partial AND clears
        // accumulated fast-waveform artifacts (ghosts / "empty white rectangles").
        self.dirty = None;
        self.settle = None;
    }

    /// Push ONLY the card band (base + ink) with a fast DU partial refresh — the
    /// scroll-step hot path. Non-blocking: if the e-ink server is behind, the frame
    /// is in shm and the next step (or the colour settle) will show it. DU (not A2)
    /// because the band is card CONTENT — grayscale/images — not just ink.
    pub fn blit_band_fast(&mut self, qtfb: &mut Qtfb) {
        let shm: &mut [u8] = &mut *qtfb.shm;
        for idx in (CARD_Y * WIDTH)..((CARD_Y + CARD_H) * WIDTH) {
            let bi = idx * 4;
            let di = idx * 3;
            let cov = self.ink[idx] as u32;
            if cov == 0 {
                shm[di] = self.base[bi];
                shm[di + 1] = self.base[bi + 1];
                shm[di + 2] = self.base[bi + 2];
            } else {
                let ia = 255 - cov;
                for k in 0..3 {
                    shm[di + k] = ((self.base[bi + k] as u32 * ia) / 255) as u8;
                }
            }
        }
        let _ = qtfb.try_set_refresh_mode(REFRESH_MODE_FAST);
        let _ = qtfb.try_update_partial(0, CARD_Y as i32, WIDTH as i32, CARD_H as i32);
        // The DU pass dropped colour over the whole band — queue a colour settle.
        self.settle = Some((0, CARD_Y as i32, (WIDTH - 1) as i32, (CARD_Y + CARD_H - 1) as i32));
        self.dirty = None; // superseded: the whole band was just pushed
    }

    /// Undo the last completed stroke: rebuild ink from the remainder, full refresh.
    /// Returns `false` when there was no stroke to undo (caller may then undo the card).
    pub fn undo(&mut self, qtfb: &mut Qtfb) -> bool {
        if self.strokes.pop().is_none() {
            return false;
        }
        self.rebuild_ink();
        self.blit_full(qtfb);
        true
    }

    /// Clear the whiteboard (button): drop ink, restore base, full refresh.
    pub fn clear(&mut self, qtfb: &mut Qtfb) {
        self.clear_ink();
        self.blit_full(qtfb);
    }

    fn rebuild_ink(&mut self) {
        for b in self.ink.iter_mut() {
            *b = 0;
        }
        let strokes = std::mem::take(&mut self.strokes);
        let (scroll, zoom) = (self.scroll, self.zoom);
        for st in &strokes {
            let mut prev: Option<(i32, i32)> = None;
            for &(cx, cy, w) in &st.pts {
                // content CSS → panel px through the CURRENT scroll/zoom. Radius
                // truncates like the capture path did, so a stroke replayed at the
                // zoom it was drawn at is pixel-identical to its live render.
                let x = (cx as f32 * zoom).round() as i32;
                let y = ((cy as f32 - scroll) * zoom).round() as i32 + CARD_Y as i32;
                let r = (((w as f32) * zoom) as i32 / 2).max(1);
                let (fx, fy) = prev.unwrap_or((x, y));
                // segment_ink clips to the card band per-stamp, so segments that
                // scrolled off-window vanish cleanly.
                segment_ink(&mut self.ink, fx, fy, x, y, r, st.erase);
                prev = Some((x, y));
            }
        }
        self.strokes = strokes;
    }

    /// Ink one pen sample into the framebuffer WITHOUT refreshing — the dirty rect
    /// is accumulated and pushed later by `flush_ink`. This split (many samples, one
    /// refresh) is what makes inking smooth: a QTFB partial-refresh per sample can't
    /// keep up with the pen's sample rate and reads as laggy/jittery.
    pub fn ink_sample(&mut self, s: PenSample, qtfb: &mut Qtfb) {
        if s.up {
            if let Some(st) = self.active.take() {
                if !st.pts.is_empty() {
                    self.strokes.push(st);
                }
            }
            self.last = None;
            return;
        }
        // The eraser end (flipped pen) OR the Eraser toggle both erase.
        let erasing = s.erase || self.eraser;
        if s.down || self.active.is_none() {
            self.active = Some(Stroke {
                erase: erasing,
                pts: Vec::new(),
            });
            self.last = None;
        }
        // Confine to the card region; leaving it breaks the current segment.
        if !in_card(s.y) {
            self.last = None;
            return;
        }
        let x = s.x.clamp(0, (WIDTH - 1) as i32);
        let y = s.y;
        let r = if erasing {
            ERASE_RADIUS
        } else {
            ((s.w as i32) / 2).max(1)
        };
        let (fx, fy) = self.last.unwrap_or((x, y));

        // 1. rasterize the segment into ink, tracking its bbox.
        let (mut minx, mut miny, mut maxx, mut maxy) = (x.min(fx), y.min(fy), x.max(fx), y.max(fy));
        segment_ink(&mut self.ink, fx, fy, x, y, r, erasing);
        let pad = r + 1;
        minx = (minx - pad).max(0);
        miny = (miny - pad).max(CARD_Y as i32);
        maxx = (maxx + pad).min((WIDTH - 1) as i32);
        maxy = (maxy + pad).min((CARD_Y + CARD_H - 1) as i32);

        // 2. recompute the framebuffer over that bbox from base+ink (no refresh).
        {
            let shm: &mut [u8] = &mut *qtfb.shm;
            for yy in miny..=maxy {
                for xx in minx..=maxx {
                    let idx = yy as usize * WIDTH + xx as usize;
                    let bi = idx * 4;
                    let di = idx * 3;
                    let cov = self.ink[idx] as u32;
                    if cov == 0 {
                        shm[di] = self.base[bi];
                        shm[di + 1] = self.base[bi + 1];
                        shm[di + 2] = self.base[bi + 2];
                    } else {
                        let ia = 255 - cov;
                        for k in 0..3 {
                            shm[di + k] = ((self.base[bi + k] as u32 * ia) / 255) as u8;
                        }
                    }
                }
            }
        }

        // 3. record the point in content CSS coords (so ink scrolls AND zooms with
        // the card). Erase strokes record too — with the effective eraser width —
        // so both view rebuilds and undo replay them faithfully.
        {
            let zoom = self.zoom;
            let cx = ((x as f32) / zoom).round() as i32;
            let cy = (((y - CARD_Y as i32) as f32) / zoom + self.scroll).round() as i32;
            let wpanel = if erasing { (ERASE_RADIUS * 2) as f32 } else { s.w as f32 };
            let wrec = (wpanel / zoom).max(1.0) as u8;
            if let Some(st) = self.active.as_mut() {
                st.pts.push((cx, cy, wrec));
            }
        }
        self.last = Some((x, y));
        self.dirty = Some(match self.dirty {
            None => (minx, miny, maxx, maxy),
            Some((dx0, dy0, dx1, dy1)) => {
                (dx0.min(minx), dy0.min(miny), dx1.max(maxx), dy1.max(maxy))
            }
        });
    }

    /// Push one coalesced fast (DU) partial refresh over everything inked since the
    /// last flush, using NON-BLOCKING sends. If the e-ink server is behind (socket
    /// buffer full) the dirty region is KEPT and retried next flush — this both
    /// prevents the mid-writing hang (a blocking send would stall the loop and let
    /// the marker event buffer back up) and rate-limits refreshes to panel speed.
    pub fn flush_ink(&mut self, qtfb: &mut Qtfb) {
        if let Some((x0, y0, x1, y1)) = self.dirty {
            // A2 waveform for ink — far lower perceived latency than DU. Mode is
            // only actually sent when it changes (see Qtfb), so a continuous stroke
            // stays in A2 with no per-flush mode messages.
            let _ = qtfb.try_set_refresh_mode(REFRESH_MODE_ANIMATE);
            if qtfb
                .try_update_partial(x0, y0, x1 - x0 + 1, y1 - y0 + 1)
                .is_ok()
            {
                // Sent with the grayscale DU waveform — remember the region so a
                // colour settle can restore it once writing pauses.
                self.settle = Some(match self.settle {
                    None => (x0, y0, x1, y1),
                    Some((sx0, sy0, sx1, sy1)) => {
                        (sx0.min(x0), sy0.min(y0), sx1.max(x1), sy1.max(y1))
                    }
                });
                self.dirty = None; // sent; otherwise keep it and retry
            }
        }
    }

    /// True while ink is waiting to be flushed (used to shorten the poll timeout so
    /// a dropped update retries promptly).
    pub fn has_pending_ink(&self) -> bool {
        self.dirty.is_some()
    }

    /// True while a fast-waveform-inked region is waiting for its colour (GC16)
    /// clean.
    pub fn needs_settle(&self) -> bool {
        self.settle.is_some()
    }

    /// Colour-clean ONLY the region inked with the fast waveform (one GC16
    /// `update_partial`, not a whole-screen refresh) — restores the card's colour
    /// and clears fast-waveform artifacts there, with a localized (not full-screen)
    /// flash. Called once writing has paused; the debounce collapses many
    /// mid-sentence pauses into a single settle. Blocking send is fine (we're idle).
    pub fn settle_color(&mut self, qtfb: &mut Qtfb) {
        if let Some((x0, y0, x1, y1)) = self.settle.take() {
            let _ = qtfb.set_refresh_mode(REFRESH_MODE_CONTENT);
            let _ = qtfb.update_partial(x0, y0, x1 - x0 + 1, y1 - y0 + 1);
        }
    }

    /// True while a pen stroke is in progress — used for palm rejection (ignore
    /// finger touches while the pen is writing).
    pub fn is_inking(&self) -> bool {
        self.active.is_some()
    }
}

impl Default for Whiteboard {
    fn default() -> Self {
        Self::new()
    }
}

/// Stamp a filled disc of radius `r` into `ink` at (cx,cy). Draw sets full
/// coverage; erase clears it. Clipped to the card region.
fn stamp(ink: &mut [u8], cx: i32, cy: i32, r: i32, erase: bool) {
    let val = if erase { 0 } else { 255 };
    let r2 = r * r;
    for dy in -r..=r {
        let y = cy + dy;
        if !in_card(y) || y < 0 || y >= HEIGHT as i32 {
            continue;
        }
        for dx in -r..=r {
            if dx * dx + dy * dy > r2 {
                continue;
            }
            let x = cx + dx;
            if x < 0 || x >= WIDTH as i32 {
                continue;
            }
            ink[y as usize * WIDTH + x as usize] = val;
        }
    }
}

/// Rasterize a thick line (stamped discs) from (x0,y0) to (x1,y1) into `ink`.
fn segment_ink(ink: &mut [u8], x0: i32, y0: i32, x1: i32, y1: i32, r: i32, erase: bool) {
    let steps = (x1 - x0).abs().max((y1 - y0).abs()).max(1);
    for i in 0..=steps {
        let x = x0 + (x1 - x0) * i / steps;
        let y = y0 + (y1 - y0) * i / steps;
        stamp(ink, x, y, r, erase);
    }
}
