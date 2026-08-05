//! ankimarkable — Anki review client for the reMarkable Paper Pro (AppLoad/QTFB).
//!
//! Review the `uni` deck on-device with Anki's real backend (rslib): true `.anki2`
//! collection, real FSRS scheduler, AnkiWeb sync, full HTML/CSS card rendering,
//! plus an AnkiDroid-style pen **whiteboard** over the card so you can write your
//! answer before revealing. External AppLoad app — no xochitl injection, no
//! bank-flip risk.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;

use ankimarkable::backend::{self, Backend, Counts, DeckInfo, Stats, SyncReport};
use ankimarkable::gesture::{CardGesture, CardTouch};
use ankimarkable::pen::Pen;
use ankimarkable::qtfb::{
    Qtfb, INPUT_TOUCH_PRESS, INPUT_TOUCH_RELEASE, INPUT_TOUCH_UPDATE, REFRESH_MODE_CONTENT,
};
use ankimarkable::render::{CardView, Renderer};
use ankimarkable::ui::{self, Hit, HomeHit, Phase};
use ankimarkable::whiteboard::Whiteboard;

static RUNNING: AtomicBool = AtomicBool::new(true);

extern "C" fn on_sigterm(_sig: i32) {
    RUNNING.store(false, Ordering::SeqCst);
}

const DATA_DIR: &str = "/home/root/.ankimarkable";

/// Which screen is showing. The app opens to Home (deck list + streak); tapping a
/// deck enters Review; the Home button returns.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Screen {
    Home,
    Review,
}

fn collection_path() -> String {
    std::env::var("ANKIMARKABLE_COL").unwrap_or_else(|_| format!("{DATA_DIR}/collection.anki2"))
}

fn main() {
    // Graceful shutdown on swipe-from-top (AppLoad sends SIGTERM).
    unsafe {
        let h = on_sigterm as extern "C" fn(i32) as libc::sighandler_t;
        libc::signal(libc::SIGTERM, h);
        libc::signal(libc::SIGINT, h);
    }

    let qtfb = match Qtfb::connect_from_env() {
        Ok(q) => q,
        Err(e) => {
            eprintln!("ankimarkable: QTFB connect failed: {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = run(qtfb) {
        eprintln!("ankimarkable: {e:#}");
    }
}

fn run(mut qtfb: Qtfb) -> Result<()> {
    let _ = std::fs::create_dir_all(DATA_DIR);
    let col_path = collection_path();
    // Card media lives in the sibling `<collection>.media` folder.
    let media_dir = std::path::Path::new(&col_path).with_extension("media");
    let renderer = Renderer::new().with_media_dir(media_dir);
    let mut wb = Whiteboard::new();

    // Read the streak BEFORE opening the collection — anki takes an exclusive lock
    // on open, after which no other connection can read `revlog`.
    let base_stats = backend::read_stats(&col_path);

    let mut backend = match Backend::open(&col_path) {
        Ok(b) => b,
        Err(e) => {
            ui::draw_message(
                &mut qtfb,
                &renderer,
                "No collection",
                &format!("{e}. Tap Exit, set up AnkiWeb creds, then Sync."),
            );
            wait_for_exit(&qtfb);
            return Ok(());
        }
    };

    // The pen ("Elan marker") is read directly from evdev; if it's unavailable the
    // app still works finger-only (buttons via the QTFB touch socket).
    let mut pen = match Pen::open() {
        Ok(p) => {
            eprintln!("ankimarkable: pen at {}", p.dev_path);
            Some(p)
        }
        Err(e) => {
            eprintln!("ankimarkable: no pen ({e}) — finger-only");
            None
        }
    };

    // App opens on the Home screen: a collapsible deck tree + streak.
    let mut screen = Screen::Home;
    let mut all_decks: Vec<DeckInfo> = backend.deck_infos().unwrap_or_default();
    // Runtime collapse state, keyed by deck id (empty = all expanded so subdecks
    // are visible; the chevron toggles per deck).
    let mut collapsed: HashSet<i64> = HashSet::new();
    let mut scroll: usize = 0;
    let mut visible = visible_decks(&all_decks, &collapsed);
    // Reviews graded this session — added to the startup `reviewed_today` (the DB
    // is locked now so we can't re-read it).
    let mut session_reviews: u32 = 0;
    // Palm rejection: touches within this window of the last pen sample are ignored.
    let mut last_pen: Option<Instant> = None;
    const PALM_WINDOW: Duration = Duration::from_millis(250);
    // …and while the pen merely HOVERS, which is the state your hand lands in when
    // you settle it on the panel to start writing. Bounded by pen recency so a
    // missing "left proximity" report can't lock out finger input permanently
    // (hovering emits a steady event stream, so a real hover keeps refreshing it).
    let mut pen_hover = false;
    const HOVER_WINDOW: Duration = Duration::from_millis(1500);
    // Idle gap after which a colour settle fires — long enough that pauses BETWEEN
    // words while handwriting a sentence don't each trigger a refresh (they collapse
    // into one settle once you actually stop).
    const SETTLE_IDLE: Duration = Duration::from_millis(1200);
    // Timestamp of the last touch event we processed. This panel drops RELEASE
    // (and pen-up) samples; a dropped gesture-end would otherwise latch
    // `multi_active()`/`is_inking()` true forever and silently swallow every later
    // tap (the "frozen app" failure mode). If the screen has been untouched for
    // STALE_IDLE while such a state is latched, we force it closed below.
    let mut last_touch: Option<Instant> = None;
    const STALE_IDLE: Duration = Duration::from_millis(2000);

    // Review state, populated when a deck is picked.
    let mut counts = Counts::default();
    let mut current: Option<backend::ReviewCard> = None;
    let mut menu_open = false; // ⋮ overflow menu (Bury card / Suspend note)
    let mut mono = false; // false = colour card rendering, true = black-and-white
    // Live scrollable/zoomable card document (None until a card is shown).
    let mut view: Option<CardView> = None;
    // Two-finger scroll/pinch recognition over the QTFB touch stream.
    let mut touch = CardTouch::new();
    let mut last_gesture: Option<Instant> = None;
    let mut phase = Phase::Question;
    let mut status = String::new();

    ui::draw_home(
        &mut qtfb,
        &renderer,
        &visible,
        scroll,
        home_stats(base_stats, session_reviews),
    );

    'main: while RUNNING.load(Ordering::SeqCst) {
        // Recover from a dropped terminal touch/pen event. This panel routinely
        // drops the RELEASE that ends a two-finger gesture and the pen-up that ends
        // a stroke; either leak latches `multi_active()`/`is_inking()` true, after
        // which EVERY tap is swallowed (as a phantom gesture / as palm) and the app
        // reads as frozen. There's no in-band signal that a held finger lifted, so
        // we time it out: if the screen has been untouched for STALE_IDLE while such
        // a state is latched, force the gesture tracker and any open stroke closed.
        // (A real gesture completes well under STALE_IDLE, so this never clips one.)
        if touch.multi_active() && last_touch.map_or(true, |t| t.elapsed() >= STALE_IDLE) {
            touch = CardTouch::new();
        }
        if wb.is_inking() && last_pen.map_or(true, |t| t.elapsed() >= STALE_IDLE) {
            wb.end_stroke();
        }
        // Flush any pending ink FIRST (non-blocking; keeps the dirty region if the
        // e-ink server is still busy). Retrying at the loop top means a deferred
        // update lands promptly without ever blocking the loop. Once writing has
        // clearly PAUSED (SETTLE_IDLE since the last pen sample — long enough that
        // mid-sentence word-pauses don't each trigger one), do ONE colour (GC16)
        // clean of just the inked REGION: the fast DU waveform used while writing is
        // grayscale + leaves "white rectangle" artifacts on the colour panel; the
        // region clean restores colour + wipes them with only a small localized
        // flash (never a whole-screen refresh). The debounce collapses the many
        // pauses in a sentence into a single settle.
        if screen == Screen::Review {
            wb.flush_ink(&mut qtfb);
            let pen_quiet = last_pen.map_or(true, |t| t.elapsed() >= SETTLE_IDLE);
            let gesture_quiet =
                last_gesture.map_or(true, |t| t.elapsed() >= Duration::from_millis(500));
            if wb.needs_settle() && pen_quiet && gesture_quiet && !touch.multi_active() {
                // The settle is a GC16 pass whose ONLY job is to restore colour and
                // wipe fast-waveform artifacts on the colour panel. In black-and-white
                // mode there's no colour to restore, so it would be a pointless flash
                // after every writing pause — drop it (also clears needs_settle() so
                // the loop returns to its idle poll cadence).
                if mono {
                    wb.discard_settle();
                } else {
                    wb.settle_color(&mut qtfb);
                }
            }
        }
        let reviewing = screen == Screen::Review;

        // Poll the QTFB touch socket and (if present) the pen evdev fd together.
        let mut fds: Vec<libc::pollfd> = Vec::with_capacity(2);
        fds.push(libc::pollfd {
            fd: qtfb.raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
        if let Some(p) = &pen {
            fds.push(libc::pollfd {
                fd: p.fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }
        // Short timeout while ink is pending so a deferred flush retries within ~30ms;
        // a medium one while a colour settle is due so it fires shortly after a pause.
        let timeout = if reviewing && wb.has_pending_ink() {
            30
        } else if reviewing && wb.needs_settle() {
            120
        } else {
            1000
        };
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
        if rc <= 0 {
            // EINTR (SIGTERM) or timeout — loop back; the top-of-loop flush retries.
            continue;
        }

        // Pen: drain ALL samples and ink them (the refresh is flushed at the loop
        // top, coalesced and non-blocking — never per-sample, never blocking).
        if fds.len() == 2 && fds[1].revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) != 0 {
            let mut inked = false;
            let mut stroke_started = false;
            // The PEN WINS over touch. Gating ink on `!touch.multi_active()` meant a
            // resting palm — which reports as ≥2 card contacts — silently swallowed
            // every sample for as long as the hand stayed down, i.e. writing simply
            // didn't work. A palm can't be told from fingers by position, but the
            // marker is separate hardware: if it's reporting, the hand on the glass
            // is a palm, so ink unconditionally and cancel the phantom gesture below.
            let r = pen.as_mut().unwrap().drain(|s| {
                inked = true;
                if reviewing {
                    stroke_started |= s.down;
                    wb.ink_sample(s, &mut qtfb);
                }
            });
            let p = pen.as_mut().unwrap();
            pen_hover = p.in_proximity();
            let pen_events = p.take_activity();
            if stroke_started {
                // Drop whatever the palm registered before the tip landed, so it can
                // neither latch `multi_active()` nor fire a scroll/pinch mid-stroke.
                touch = CardTouch::new();
            }
            // Stamp on ANY marker traffic, hover included — the palm-rejection window
            // must already be open when the hand lands ahead of the first ink sample.
            if inked || pen_events {
                last_pen = Some(Instant::now());
            }
            if inked && reviewing {
                // Ship the freshly-inked pixels NOW (same iteration) rather than
                // waiting for the next poll cycle — one less cycle of ink latency.
                wb.flush_ink(&mut qtfb);
            }
            if r.is_err() {
                eprintln!("ankimarkable: pen read error — dropping pen");
                pen = None;
            }
        }

        if fds[0].revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) == 0 {
            continue;
        }
        // Drain EVERY queued touch event this wake (motion arrives as repeated
        // PRESS with the same dev_id on this panel — one-event-per-poll would lag).
        'input: loop {
            let ev = match qtfb.recv_input_nonblock() {
                Ok(Some(ev)) => ev,
                Ok(None) => break 'input,
                Err(_) => break 'main, // socket closed
            };
            let ty = ev.input_type;
            let is_touch = ty == INPUT_TOUCH_PRESS
                || ty == INPUT_TOUCH_RELEASE
                || ty == INPUT_TOUCH_UPDATE;
            if !is_touch {
                continue 'input;
            }
            // Palm rejection: ignore finger/palm touches while a stroke is in
            // progress, within PALM_WINDOW of the last pen sample, and while the pen
            // hovers (the hand settles on the panel BEFORE the tip lands, so without
            // the hover arm the palm gets a head start and registers as a gesture).
            let pen_recent = last_pen.map_or(false, |t| t.elapsed() < PALM_WINDOW);
            let hovering = pen_hover && last_pen.map_or(false, |t| t.elapsed() < HOVER_WINDOW);
            if reviewing && (pen_recent || hovering || wb.is_inking()) {
                // A swallowed RELEASE must still retire its contact — otherwise the
                // palm stays "down" in the tracker and latches multi_active() on.
                if ty == INPUT_TOUCH_RELEASE {
                    touch.forget(ev.dev_id);
                }
                continue 'input;
            }
            // Stamp only touches we actually act on: the stale-gesture recovery at the
            // loop top must be able to fire while a rejected palm keeps streaming.
            last_touch = Some(Instant::now());

            // ── Review-screen gesture routing (menu-open taps skip to hit_menu) ──
            if reviewing && !menu_open {
                let in_card = ui::in_card_region(ev.x, ev.y);
                if in_card || touch.is_tracked(ev.dev_id) || touch.multi_active() {
                    let fed = touch.feed(ty, ev.dev_id, ev.x, ev.y, in_card);
                    match fed.gesture {
                        CardGesture::ScrollStep(dy) => {
                            if let Some(v) = view.as_mut() {
                                if v.scroll_by(dy as f64) {
                                    let mut window = v.paint_window();
                                    ui::draw_scroll_indicator(
                                        &mut window,
                                        v.scroll_phys(),
                                        v.max_scroll(),
                                        v.content_h(),
                                    );
                                    ui::patch_card_band(
                                        wb.base_mut(),
                                        &window,
                                        &renderer,
                                        menu_open,
                                        mono,
                                    );
                                    wb.set_view(v.scroll_css() as f32, v.zoom() as f32);
                                    wb.blit_band_fast(&mut qtfb);
                                    last_gesture = Some(Instant::now());
                                }
                            }
                        }
                        CardGesture::PinchEnd { ratio } => {
                            let dir = if ratio >= 1.15 {
                                1
                            } else if ratio <= 0.87 {
                                -1
                            } else {
                                0
                            };
                            if dir != 0 {
                                if let Some(v) = view.as_mut() {
                                    if v.set_zoom_step(dir) {
                                        // Re-anchor the ink to the new scroll/zoom
                                        // BEFORE the band blit so it ships one frame.
                                        wb.set_view(v.scroll_css() as f32, v.zoom() as f32);
                                        let mut window = v.paint_window();
                                        ui::draw_scroll_indicator(
                                            &mut window,
                                            v.scroll_phys(),
                                            v.max_scroll(),
                                            v.content_h(),
                                        );
                                        ui::patch_card_band(
                                            wb.base_mut(),
                                            &window,
                                            &renderer,
                                            menu_open,
                                            mono,
                                        );
                                        // Fast feedback now; the queued colour settle
                                        // makes it crisp ~500ms after the gesture.
                                        wb.blit_band_fast(&mut qtfb);
                                        last_gesture = Some(Instant::now());
                                    }
                                }
                            }
                        }
                        CardGesture::None => {}
                    }
                    if touch.multi_active() {
                        last_gesture = Some(Instant::now());
                    }
                    continue 'input; // card-region touches NEVER hit-test (palm safety)
                }
                // Chrome buttons: feed for dev_id dedup; act once per physical press
                // (motion re-PRESSes of a held finger must not re-fire buttons).
                let fed = touch.feed(ty, ev.dev_id, ev.x, ev.y, false);
                if ty != INPUT_TOUCH_PRESS || !fed.new_contact {
                    continue 'input;
                }
            } else if ty != INPUT_TOUCH_PRESS {
                // Home screen + open-menu handling act on plain presses only.
                continue 'input;
            }

            match screen {
            Screen::Home => match ui::hit_test_home(ev.x, ev.y, &visible, scroll) {
                HomeHit::Deck(i) => {
                    if let Some(d) = visible.get(i) {
                        let _ = backend.set_deck_scope_id(d.id);
                        counts = backend.counts().unwrap_or_default();
                        current = backend.next_card().unwrap_or(None);
                        phase = Phase::Question;
                        status = d.name.clone();
                        wb.clear_ink();
                        invalidate_view(&mut view, &mut wb);
                        screen = Screen::Review;
                        redraw(&mut qtfb, &renderer, &mut wb, &current, &mut view, phase, counts, &status, menu_open, mono);
                    }
                }
                HomeHit::Toggle(i) => {
                    if let Some(d) = visible.get(i) {
                        let id = d.id.0;
                        if !collapsed.remove(&id) {
                            collapsed.insert(id);
                        }
                        visible = visible_decks(&all_decks, &collapsed);
                        clamp_scroll(&mut scroll, visible.len());
                        ui::draw_home(
                            &mut qtfb,
                            &renderer,
                            &visible,
                            scroll,
                            home_stats(base_stats, session_reviews),
                        );
                    }
                }
                HomeHit::ScrollUp => {
                    let step = ui::home_page_rows().saturating_sub(1).max(1);
                    scroll = scroll.saturating_sub(step);
                    ui::draw_home(
                        &mut qtfb,
                        &renderer,
                        &visible,
                        scroll,
                        home_stats(base_stats, session_reviews),
                    );
                }
                HomeHit::ScrollDown => {
                    let step = ui::home_page_rows().saturating_sub(1).max(1);
                    let max_scroll = visible.len().saturating_sub(ui::home_page_rows());
                    scroll = (scroll + step).min(max_scroll);
                    ui::draw_home(
                        &mut qtfb,
                        &renderer,
                        &visible,
                        scroll,
                        home_stats(base_stats, session_reviews),
                    );
                }
                HomeHit::Sync => {
                    ui::draw_message(
                        &mut qtfb,
                        &renderer,
                        "Syncing…",
                        "Contacting AnkiWeb — this can take a moment.",
                    );
                    let report = do_sync(&mut backend);
                    let body = sync_report_body(&report);
                    eprintln!(
                        "ankimarkable: sync from home: {} | {}",
                        report.headline,
                        body.replace('\n', " | ")
                    );
                    // Hold the report on screen until acknowledged — the counts
                    // are the only feedback that the sync actually moved cards.
                    let msg = if body.is_empty() {
                        "Tap anywhere to continue.".to_string()
                    } else {
                        format!("{body}\n\nTap anywhere to continue.")
                    };
                    ui::draw_message(&mut qtfb, &renderer, &report.headline, &msg);
                    wait_for_tap(&mut qtfb);
                    all_decks = backend.deck_infos().unwrap_or_default();
                    visible = visible_decks(&all_decks, &collapsed);
                    clamp_scroll(&mut scroll, visible.len());
                    ui::draw_home(
                        &mut qtfb,
                        &renderer,
                        &visible,
                        scroll,
                        home_stats(base_stats, session_reviews),
                    );
                }
                HomeHit::Exit => break 'main,
                HomeHit::None => {}
            },
            // While the ⋮ menu is open, taps hit its items (or dismiss it).
            Screen::Review if menu_open => {
                let act = ui::hit_menu(ev.x, ev.y);
                menu_open = false;
                let advanced = match act {
                    ui::MenuHit::Bury => {
                        if let Some(card) = &current {
                            if let Err(e) = backend.bury_card(card.card_id) {
                                status = format!("bury error: {e}");
                            }
                        }
                        true
                    }
                    ui::MenuHit::Suspend => {
                        if let Some(card) = &current {
                            if let Err(e) = backend.suspend_note(card.note_id) {
                                status = format!("suspend error: {e}");
                            }
                        }
                        true
                    }
                    ui::MenuHit::None => false, // tapped outside → just dismiss
                };
                if advanced {
                    counts = backend.counts().unwrap_or_default();
                    current = backend.next_card().unwrap_or(None);
                    phase = Phase::Question;
                    wb.clear_ink();
                    invalidate_view(&mut view, &mut wb);
                }
                redraw(&mut qtfb, &renderer, &mut wb, &current, &mut view, phase, counts, &status, menu_open, mono);
            }
            Screen::Review => match ui::hit_test(ev.x, ev.y, phase) {
                Hit::Exit => break 'main,
                Hit::Home => {
                    // Back to the deck list; rebuild deck counts (streak is cached).
                    invalidate_view(&mut view, &mut wb);
                    touch = CardTouch::new();
                    screen = Screen::Home;
                    all_decks = backend.deck_infos().unwrap_or_default();
                    visible = visible_decks(&all_decks, &collapsed);
                    clamp_scroll(&mut scroll, visible.len());
                    wb.clear_ink();
                    ui::draw_home(
                        &mut qtfb,
                        &renderer,
                        &visible,
                        scroll,
                        home_stats(base_stats, session_reviews),
                    );
                }
                Hit::Sync => {
                    ui::draw_message(
                        &mut qtfb,
                        &renderer,
                        "Syncing…",
                        "Contacting AnkiWeb — this can take a moment.",
                    );
                    let report = do_sync(&mut backend);
                    let body = sync_report_body(&report);
                    eprintln!(
                        "ankimarkable: sync from review: {} | {}",
                        report.headline,
                        body.replace('\n', " | ")
                    );
                    let msg = if body.is_empty() {
                        "Tap anywhere to continue.".to_string()
                    } else {
                        format!("{body}\n\nTap anywhere to continue.")
                    };
                    ui::draw_message(&mut qtfb, &renderer, &report.headline, &msg);
                    wait_for_tap(&mut qtfb);
                    status = report.headline;
                    counts = backend.counts().unwrap_or_default();
                    current = backend.next_card().unwrap_or(None);
                    phase = Phase::Question;
                    wb.clear_ink();
                    invalidate_view(&mut view, &mut wb);
                    redraw(&mut qtfb, &renderer, &mut wb, &current, &mut view, phase, counts, &status, menu_open, mono);
                }
                Hit::ShowAnswer => {
                    if current.is_some() && phase == Phase::Question {
                        phase = Phase::Answer;
                        invalidate_view(&mut view, &mut wb);
                        // Keep ink so you can compare your answer against the card.
                        redraw(&mut qtfb, &renderer, &mut wb, &current, &mut view, phase, counts, &status, menu_open, mono);
                    }
                }
                Hit::Grade(g) => {
                    if phase == Phase::Answer {
                        if let Some(card) = &current {
                            match backend.answer(card, g) {
                                Ok(()) => session_reviews += 1,
                                Err(e) => status = format!("answer error: {e}"),
                            }
                        }
                        counts = backend.counts().unwrap_or_default();
                        current = backend.next_card().unwrap_or(None);
                        phase = Phase::Question;
                        wb.clear_ink(); // next card starts on a clean whiteboard
                        invalidate_view(&mut view, &mut wb);
                        redraw(&mut qtfb, &renderer, &mut wb, &current, &mut view, phase, counts, &status, menu_open, mono);
                    }
                }
                Hit::Undo => {
                    // Undo the last pen stroke; when the whiteboard is empty, fall
                    // through to undoing the last graded card (Anki collection undo).
                    if !wb.undo(&mut qtfb) {
                        match backend.undo() {
                            Ok(true) => {
                                session_reviews = session_reviews.saturating_sub(1);
                                counts = backend.counts().unwrap_or_default();
                                current = backend.next_card().unwrap_or(None);
                                phase = Phase::Question;
                                wb.clear_ink();
                                invalidate_view(&mut view, &mut wb);
                                redraw(
                                    &mut qtfb, &renderer, &mut wb, &current, &mut view, phase,
                                    counts, &status, menu_open, mono,
                                );
                            }
                            _ => {} // nothing to undo
                        }
                    }
                }
                Hit::Clear => wb.clear(&mut qtfb),
                Hit::EraserToggle => {
                    wb.toggle_eraser();
                    // Recompose so the toolbar reflects the toggle; ink is kept.
                    redraw(&mut qtfb, &renderer, &mut wb, &current, &mut view, phase, counts, &status, menu_open, mono);
                }
                Hit::Menu => {
                    // Open the ⋮ overflow menu (Bury card / Suspend note).
                    menu_open = true;
                    redraw(&mut qtfb, &renderer, &mut wb, &current, &mut view, phase, counts, &status, menu_open, mono);
                }
                Hit::ColorToggle => {
                    // Tap the counts (top-left) to flip colour / black-and-white.
                    mono = !mono;
                    redraw(&mut qtfb, &renderer, &mut wb, &current, &mut view, phase, counts, &status, menu_open, mono);
                }
                Hit::None => {}
            },
            }
        } // 'input drain
    }

    // Drop(Qtfb) sends MESSAGE_TERMINATE.
    Ok(())
}

/// Compose the current screen into the whiteboard's base frame and push it (base +
/// any ink) to the panel with a clean full refresh.
///
/// `view` is the live scrollable/zoomable card document — built here on demand
/// (when a card/phase change invalidated it to `None`) and painted at its current
/// scroll/zoom. Callers that change WHICH html is shown must set `*view = None`
/// (see `invalidate_view`) so the document rebuilds.
fn redraw(
    qtfb: &mut Qtfb,
    renderer: &Renderer,
    wb: &mut Whiteboard,
    current: &Option<backend::ReviewCard>,
    view: &mut Option<CardView>,
    phase: Phase,
    counts: Counts,
    status: &str,
    menu_open: bool,
    mono: bool,
) {
    let eraser = wb.eraser;
    let frame = match current {
        Some(card) => {
            let html = match phase {
                Phase::Question => &card.question_html,
                Phase::Answer => &card.answer_html,
            };
            if view.is_none() {
                *view =
                    Some(renderer.build_card_view(html, ui::WIDTH as u32, ui::CARD_H as u32));
            }
            let v = view.as_ref().unwrap();
            let mut window = v.paint_window();
            ui::draw_scroll_indicator(&mut window, v.scroll_phys(), v.max_scroll(), v.content_h());
            ui::compose_review(
                renderer, &window, card, phase, counts, status, eraser, menu_open, mono,
            )
        }
        // Keep the top menu bar so Home / Sync / Exit still work (an empty deck or
        // a finished one must not strand you on a chrome-less screen).
        None => ui::compose_review_empty(
            renderer,
            counts,
            status,
            "All done",
            "No more cards due. Tap Sync to fetch, or Home for another deck.",
        ),
    };
    wb.set_base(frame);
    wb.blit_full(qtfb);
}

/// The shown card html is changing (new card / phase flip / leaving review):
/// drop the live view and reset the whiteboard's scroll/zoom anchoring.
fn invalidate_view(view: &mut Option<CardView>, wb: &mut Whiteboard) {
    *view = None;
    wb.set_view(0.0, 1.0);
}

/// Filter the full deck tree to the rows visible given the runtime collapse state:
/// a deck is hidden when any ancestor is collapsed. Each row's `collapsed` field is
/// set to the runtime state so the chevron renders correctly.
fn visible_decks(all: &[DeckInfo], collapsed: &HashSet<i64>) -> Vec<DeckInfo> {
    let mut out = Vec::new();
    let mut hide: Option<u32> = None; // hide decks deeper than this level
    for d in all {
        if let Some(h) = hide {
            if d.level > h {
                continue;
            }
            hide = None;
        }
        let is_collapsed = collapsed.contains(&d.id.0);
        let mut row = d.clone();
        row.collapsed = is_collapsed;
        out.push(row);
        if is_collapsed && d.has_children {
            hide = Some(d.level);
        }
    }
    out
}

/// Keep `scroll` within range after the visible list changes.
fn clamp_scroll(scroll: &mut usize, len: usize) {
    let max_scroll = len.saturating_sub(ui::home_page_rows());
    if *scroll > max_scroll {
        *scroll = max_scroll;
    }
}

/// Combine the startup streak with this session's grades: `reviewed_today` adds
/// the in-session count, and if nothing had been studied today at startup, the
/// first grade extends the streak by one (today now counts).
fn home_stats(base: Stats, session_reviews: u32) -> Stats {
    let studied_today_at_start = base.reviewed_today > 0;
    Stats {
        reviewed_today: base.reviewed_today + session_reviews,
        streak: if !studied_today_at_start && session_reviews > 0 {
            base.streak + 1
        } else {
            base.streak
        },
    }
}

/// Read AnkiWeb credentials from `DATA_DIR/ankiweb.txt` (line 1 = email/user,
/// line 2 = password) and run a sync. Error/credential problems come back as a
/// report whose headline is the message (details all None).
fn do_sync(backend: &mut Backend) -> SyncReport {
    let creds_path = format!("{DATA_DIR}/ankiweb.txt");
    let raw = match std::fs::read_to_string(&creds_path) {
        Ok(s) => s,
        Err(_) => {
            return SyncReport {
                headline: "no AnkiWeb creds (ankiweb.txt)".to_string(),
                ..Default::default()
            }
        }
    };
    let mut lines = raw.lines();
    let (Some(user), Some(pass)) = (lines.next(), lines.next()) else {
        return SyncReport {
            headline: "ankiweb.txt needs user + pass lines".to_string(),
            ..Default::default()
        };
    };
    match backend.sync(user.trim(), pass.trim()) {
        Ok(r) => r,
        Err(e) => SyncReport {
            headline: format!("sync failed: {e}"),
            ..Default::default()
        },
    }
}

/// The Some-detail lines of a sync report, newline-joined for the message body
/// (and `|`-joined for logging). Empty when the sync had nothing to say.
fn sync_report_body(r: &SyncReport) -> String {
    [&r.added, &r.removed, &r.media, &r.server_message]
        .into_iter()
        .flatten()
        .cloned()
        .collect::<Vec<_>>()
        .join("\n")
}

fn wait_for_exit(qtfb: &Qtfb) {
    while RUNNING.load(Ordering::SeqCst) {
        match qtfb.poll_input() {
            Ok(Some(ev)) => {
                if ev.input_type == INPUT_TOUCH_PRESS {
                    if let Hit::Exit = ui::hit_test(ev.x, ev.y, Phase::Question) {
                        break;
                    }
                }
            }
            Ok(None) => continue,
            Err(_) => break,
        }
    }
}

/// Block on a "tap anywhere to continue" screen: returns on any touch press (or
/// shutdown). UPDATE/RELEASE are ignored; the pen fd isn't polled here — its
/// samples just queue and get drained on the next main-loop iteration.
fn wait_for_tap(qtfb: &mut Qtfb) {
    while RUNNING.load(Ordering::SeqCst) {
        match qtfb.poll_input() {
            Ok(Some(ev)) => {
                if ev.input_type == INPUT_TOUCH_PRESS {
                    break;
                }
            }
            Ok(None) => continue,
            Err(_) => break,
        }
    }
}
