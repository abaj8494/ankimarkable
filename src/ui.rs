//! Review UI — screen layout, HTML chrome generation, and touch hit-testing.
//!
//! The screen is composited from three independent blitz renders blitted into the
//! QTFB framebuffer: a top status bar, the card body (Anki's own HTML/CSS), and a
//! bottom action bar. Keeping the card in its own document stops Anki's per-deck
//! `body{}`/`.card{}` CSS from bleeding into our chrome. Hit-rects are defined here
//! in Rust to match the geometry of the HTML we generate.

use crate::backend::{CardKind, Counts, DeckInfo, Grade, ReviewCard, Stats};
use crate::qtfb::{Qtfb, REFRESH_MODE_CONTENT};
use crate::render::Renderer;

pub const WIDTH: usize = 1620;
pub const HEIGHT: usize = 2160;
pub const TOP_H: usize = 110;
pub const BOTTOM_H: usize = 240;
pub const CARD_Y: usize = TOP_H;
pub const CARD_H: usize = HEIGHT - TOP_H - BOTTOM_H;

// Review top-bar buttons, right-to-left: [counts][Home][Undo][Eraser][Clear][Sync][Exit].
const HOME_W: usize = 160;
const UNDO_W: usize = 160;
const ERASE_W: usize = 180;
const CLEAR_W: usize = 160;
const SYNC_W: usize = 190;
const EXIT_W: usize = 160;

// ⋮ overflow menu: a floating button in the card's bottom-right corner, with a
// popup panel above it (root actions, or the Flag… submenu).
pub const MENU_BTN: usize = 104;
const MENU_GAP: usize = 30; // inset from the right edge / bottom action bar
const MENU_PANEL_W: usize = 520;
const MENU_ITEM_H: usize = 118;

/// Which ⋮ panel is showing (`Closed` = none).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MenuState {
    Closed,
    Root,
    Flags,
}

/// Anki's seven card flags: (flag number, name, swatch colour). Single source of
/// truth — BOTH the Flags panel's HTML rows and hit_menu's row indices derive
/// from this table (in this order), so the drawn rows and the hit-test rows
/// cannot drift.
const FLAGS: [(u32, &str, &str); 7] = [
    (1, "Red", "#d32f2f"),
    (2, "Orange", "#f57c00"),
    (3, "Green", "#388e3c"),
    (4, "Blue", "#1976d2"),
    (5, "Pink", "#e91e63"),
    (6, "Turquoise", "#00bcd4"),
    (7, "Purple", "#7b1fa2"),
];

/// Rows in the panel for a given state. Root: Flag… / Bury card / Suspend note.
/// Flags: the seven flags + Remove flag.
fn menu_items(state: MenuState) -> usize {
    match state {
        MenuState::Closed => 0,
        MenuState::Root => 3,
        MenuState::Flags => FLAGS.len() + 1,
    }
}

fn menu_btn_rect() -> (usize, usize, usize, usize) {
    let x1 = WIDTH - MENU_GAP;
    let x0 = x1 - MENU_BTN;
    let y1 = HEIGHT - BOTTOM_H - MENU_GAP;
    let y0 = y1 - MENU_BTN;
    (x0, y0, x1, y1)
}

fn menu_panel_rect(items: usize) -> (usize, usize, usize, usize) {
    let (_, by0, bx1, _) = menu_btn_rect();
    let x1 = bx1;
    let x0 = x1 - MENU_PANEL_W;
    let y1 = by0 - 14;
    // Even the tallest panel (8 × 118px = 944px) starts at y0 = 828, well below
    // CARD_Y (110) — it always fits inside the card band above the anchor.
    let y0 = y1 - MENU_ITEM_H * items;
    (x0, y0, x1, y1)
}

/// Hit-test a tap while the ⋮ menu is open: a panel item, else None (dismiss).
/// Row indices are matched exhaustively per state — no catch-all mapping — so a
/// tap outside the drawn rows can never fire an action.
pub fn hit_menu(x: i32, y: i32, state: MenuState) -> MenuHit {
    if x < 0 || y < 0 {
        return MenuHit::None;
    }
    let (x, y) = (x as usize, y as usize);
    let (px0, py0, px1, py1) = menu_panel_rect(menu_items(state));
    if x >= px0 && x < px1 && y >= py0 && y < py1 {
        let row = (y - py0) / MENU_ITEM_H;
        return match state {
            MenuState::Closed => MenuHit::None,
            MenuState::Root => match row {
                0 => MenuHit::OpenFlags,
                1 => MenuHit::Bury,
                2 => MenuHit::Suspend,
                _ => MenuHit::None,
            },
            // Same order as draw_menu_panel: the FLAGS rows, then Remove flag.
            MenuState::Flags => match row {
                r if r < FLAGS.len() => MenuHit::Flag(FLAGS[r].0),
                r if r == FLAGS.len() => MenuHit::Flag(0),
                _ => MenuHit::None,
            },
        };
    }
    MenuHit::None
}

// Home-screen deck list geometry (hit_test_home must match compose_home's CSS).
pub const HOME_ROW_H: usize = 116;
const HOME_INDENT: usize = 46; // px per tree level
const HOME_PAD_L: usize = 32; // list left padding
const HOME_CHEV_W: usize = 84; // chevron tap zone width (after indent)
const SCROLL_BTN_W: usize = 88; // ▲/▼ header buttons
/// Visible deck rows that fit under the header.
pub fn home_page_rows() -> usize {
    (HEIGHT - TOP_H) / HOME_ROW_H
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Question,
    Answer,
}

#[derive(Clone, Copy, Debug)]
pub enum Hit {
    ShowAnswer,
    Grade(Grade),
    Sync,
    Exit,
    Home,
    Undo,
    Clear,
    EraserToggle,
    /// The ⋮ overflow button (bottom-right) was tapped.
    Menu,
    /// The counts area (top-left) was tapped — toggle colour / black-and-white.
    ColorToggle,
    None,
}

/// A tap while the ⋮ overflow menu is open.
#[derive(Clone, Copy, Debug)]
pub enum MenuHit {
    /// Root row "Flag…" — switch the panel to the flags submenu (stays open).
    OpenFlags,
    Bury,
    Suspend,
    /// A flags-submenu row: 1-7 sets that flag, 0 = "Remove flag".
    Flag(u32),
    /// Tapped outside the panel — dismiss it.
    None,
}

/// A tap on the home screen. `Deck`/`Toggle` carry the absolute index into the
/// visible deck list.
#[derive(Clone, Copy, Debug)]
pub enum HomeHit {
    Deck(usize),
    Toggle(usize),
    Sync,
    Exit,
    ScrollUp,
    ScrollDown,
    None,
}

/// Map a touch in physical panel coords to a UI action for the current phase.
pub fn hit_test(x: i32, y: i32, phase: Phase) -> Hit {
    if x < 0 || y < 0 {
        return Hit::None;
    }
    let (x, y) = (x as usize, y as usize);

    // Top status bar: [counts .... Home | Undo | Eraser | Clear | Sync | Exit]
    // (right-to-left, must mirror top_bar_html's button order).
    if y < TOP_H {
        let mut edge = WIDTH;
        edge -= EXIT_W;
        if x >= edge {
            return Hit::Exit;
        }
        edge -= SYNC_W;
        if x >= edge {
            return Hit::Sync;
        }
        edge -= CLEAR_W;
        if x >= edge {
            return Hit::Clear;
        }
        edge -= ERASE_W;
        if x >= edge {
            return Hit::EraserToggle;
        }
        edge -= UNDO_W;
        if x >= edge {
            return Hit::Undo;
        }
        edge -= HOME_W;
        if x >= edge {
            return Hit::Home;
        }
        // Everything left of the buttons is the counts / status area — tap it to
        // toggle colour vs black-and-white card rendering.
        return Hit::ColorToggle;
    }

    // Bottom action bar.
    if y >= HEIGHT - BOTTOM_H {
        return match phase {
            Phase::Question => Hit::ShowAnswer,
            Phase::Answer => {
                let q = WIDTH / 4;
                let idx = (x / q).min(3);
                Hit::Grade(match idx {
                    0 => Grade::Again,
                    1 => Grade::Hard,
                    2 => Grade::Good,
                    _ => Grade::Easy,
                })
            }
        };
    }

    // The ⋮ overflow button floats in the card's bottom-right corner — a small,
    // deliberate target checked BEFORE the card-region palm ignore below.
    let (mx0, my0, mx1, my1) = menu_btn_rect();
    if x >= mx0 && x < mx1 && y >= my0 && y < my1 {
        return Hit::Menu;
    }

    // Card region: IGNORE touches. The pen draws here and the palm rests here while
    // writing — a card tap must NOT reveal the answer or act (that fired "show
    // answer on its own"). Reveal is the deliberate Show Answer button only.
    let _ = phase;
    Hit::None
}

/// Is (x,y) inside the scrollable card region? Excludes chrome AND the floating
/// ⋮ button (which must stay tappable — gesture routing consumes card-region
/// touches before hit_test runs).
pub fn in_card_region(x: i32, y: i32) -> bool {
    if x < 0 || (x as usize) >= WIDTH || y < CARD_Y as i32 || (y as usize) >= HEIGHT - BOTTOM_H {
        return false;
    }
    let (mx0, my0, mx1, my1) = menu_btn_rect();
    let (xu, yu) = (x as usize, y as usize);
    !(xu >= mx0 && xu < mx1 && yu >= my0 && yu < my1)
}

/// Composite a full review frame into a fresh WIDTH×HEIGHT RGBA buffer (white bg).
/// `card_window` is the pre-painted card region (WIDTH×CARD_H RGBA) from the live
/// `CardView` — the caller owns scroll/zoom state, we just composite.
pub fn compose_review(
    renderer: &Renderer,
    card_window: &[u8],
    card: &ReviewCard,
    phase: Phase,
    counts: Counts,
    status: &str,
    eraser: bool,
    menu: MenuState,
    mono: bool,
) -> Vec<u8> {
    let mut fb = vec![255u8; WIDTH * HEIGHT * 4];

    blit(&mut fb, card_window, WIDTH, CARD_H, 0, CARD_Y);

    let top_buf = renderer.render_rgba(
        &top_bar_html(counts, Some(card.kind), status, eraser),
        WIDTH as u32,
        TOP_H as u32,
    );
    blit(&mut fb, &top_buf, WIDTH, TOP_H, 0, 0);

    let bot_buf =
        renderer.render_rgba(&bottom_bar_html(card, phase), WIDTH as u32, BOTTOM_H as u32);
    blit(&mut fb, &bot_buf, WIDTH, BOTTOM_H, 0, HEIGHT - BOTTOM_H);

    hline_black(&mut fb, CARD_Y - 2, 2);
    hline_black(&mut fb, HEIGHT - BOTTOM_H, 2);

    // ⋮ overflow button (always) + its popup panel (when open), drawn last so they
    // sit on top of the card.
    draw_menu_button(&mut fb, renderer);
    if menu != MenuState::Closed {
        draw_menu_panel(&mut fb, renderer, menu, card.flags);
    }
    // Black-and-white mode: desaturate the WHOLE composed frame (card + chrome) so
    // the toggle is unmistakable even on an already-monochrome card.
    if mono {
        desaturate(&mut fb);
    }
    fb
}

/// Patch ONLY the card band of an existing composed frame (`wb.base`) with a
/// freshly painted card window — used by scroll/zoom steps so the chrome isn't
/// re-rendered per step. Re-draws the ⋮ button (it floats over the card) and
/// re-applies black-and-white mode to the band.
pub fn patch_card_band(
    fb: &mut [u8],
    card_window: &[u8],
    renderer: &Renderer,
    menu: MenuState,
    current_flag: u8,
    mono: bool,
) {
    // Reset the band to white, then alpha-composite the window (matches the
    // full-compose path, which blits over a white frame).
    let band = &mut fb[CARD_Y * WIDTH * 4..(CARD_Y + CARD_H) * WIDTH * 4];
    for px in band.chunks_exact_mut(4) {
        px.copy_from_slice(&[255, 255, 255, 255]);
    }
    blit(fb, card_window, WIDTH, CARD_H, 0, CARD_Y);
    draw_menu_button(fb, renderer);
    if menu != MenuState::Closed {
        draw_menu_panel(fb, renderer, menu, current_flag);
    }
    if mono {
        desaturate(&mut fb[CARD_Y * WIDTH * 4..(CARD_Y + CARD_H) * WIDTH * 4]);
    }
}

/// Draw a thin scroll-position indicator along the card window's right edge
/// (drawn into the WIDTH×CARD_H window buffer itself, before compositing).
pub fn draw_scroll_indicator(window: &mut [u8], scroll: f64, max_scroll: f64, content_h: f64) {
    if max_scroll <= 0.0 || content_h <= 0.0 {
        return;
    }
    let track_h = CARD_H as f64;
    let thumb_h = ((track_h / content_h) * track_h).max(60.0);
    let thumb_y = (scroll / max_scroll) * (track_h - thumb_h);
    let (y0, y1) = (thumb_y as usize, (thumb_y + thumb_h) as usize);
    let (x0, x1) = (WIDTH - 8, WIDTH - 2);
    for y in y0..y1.min(CARD_H) {
        for x in x0..x1 {
            let i = (y * WIDTH + x) * 4;
            window[i] = 60;
            window[i + 1] = 60;
            window[i + 2] = 60;
            window[i + 3] = 255;
        }
    }
}

/// Desaturate an RGBA buffer in place (luminance grey) for black-and-white mode.
fn desaturate(buf: &mut [u8]) {
    for px in buf.chunks_exact_mut(4) {
        // Rec.601 luma; keep alpha.
        let l = ((px[0] as u32 * 299 + px[1] as u32 * 587 + px[2] as u32 * 114) / 1000) as u8;
        px[0] = l;
        px[1] = l;
        px[2] = l;
    }
}

/// The floating ⋮ button in the card's bottom-right corner.
fn draw_menu_button(fb: &mut [u8], renderer: &Renderer) {
    let (x0, y0, _, _) = menu_btn_rect();
    let html = format!(
        "<!DOCTYPE html><html><head><style>body{{margin:0;padding:0;}}\
         .m{{width:{n}px;height:{n}px;background:#37474f;color:#fff;box-sizing:border-box;\
         display:flex;align-items:center;justify-content:center;\
         font-size:82px;font-weight:700;border-radius:18px;}}</style></head>\
         <body><div class=\"m\">\u{22EE}</div></body></html>",
        n = MENU_BTN
    );
    let buf = renderer.render_rgba(&html, MENU_BTN as u32, MENU_BTN as u32);
    blit(fb, &buf, MENU_BTN, MENU_BTN, x0, y0);
}

/// The popup panel above the ⋮ button. Root: Flag… / Bury card / Suspend note;
/// Flags: one row per `FLAGS` entry (colour swatch + "{n} {Name}", ✓ + bold on
/// the current flag) then "Remove flag". Row ORDER must match `hit_menu` — both
/// are generated from the same `FLAGS` table.
fn draw_menu_panel(fb: &mut [u8], renderer: &Renderer, state: MenuState, current_flag: u8) {
    let items = menu_items(state);
    if items == 0 {
        return;
    }
    let (x0, y0, x1, y1) = menu_panel_rect(items);
    let (w, h) = (x1 - x0, y1 - y0);
    let mut rows = String::new();
    match state {
        MenuState::Closed => return,
        MenuState::Root => {
            // Surface the current flag on the submenu row so you can see the
            // card's flag without opening it.
            let flag_row = match FLAGS.iter().find(|f| f.0 == current_flag as u32) {
                Some((_, name, _)) => format!("Flag\u{2026} ({name}) \u{25B8}"),
                None => "Flag\u{2026} \u{25B8}".to_string(),
            };
            rows.push_str(&format!("<div class=\"it sep\">{flag_row}</div>"));
            rows.push_str("<div class=\"it sep\">Bury card</div>");
            rows.push_str("<div class=\"it\">Suspend note</div>");
        }
        MenuState::Flags => {
            for &(n, name, hex) in FLAGS.iter() {
                let (cur, tick) = if n == current_flag as u32 {
                    (" cur", " \u{2713}")
                } else {
                    ("", "")
                };
                rows.push_str(&format!(
                    "<div class=\"it sep{cur}\"><span class=\"sw\" \
                     style=\"background:{hex};\"></span>{n} {name}{tick}</div>"
                ));
            }
            rows.push_str("<div class=\"it\">Remove flag</div>");
        }
    }
    let html = format!(
        "<!DOCTYPE html><html><head><style>body{{margin:0;padding:0;}}\
         .p{{width:{w}px;height:{h}px;background:#fff;border:3px solid #333;box-sizing:border-box;}}\
         .it{{height:{ih}px;display:flex;align-items:center;padding-left:36px;\
         font-size:46px;color:#111;box-sizing:border-box;}}\
         .sep{{border-bottom:2px solid #cccccc;}}\
         .cur{{font-weight:700;}}\
         .sw{{display:inline-block;width:34px;height:34px;flex:none;\
         border:2px solid #888;margin-right:26px;}}</style></head>\
         <body><div class=\"p\">{rows}</div></body></html>",
        w = w,
        h = h,
        ih = MENU_ITEM_H,
        rows = rows,
    );
    let buf = renderer.render_rgba(&html, w as u32, h as u32);
    blit(fb, &buf, w, h, x0, y0);
}

/// Composite + push a full review frame (renders the card window itself at
/// scroll 0 / zoom 1 — the app's live path goes through `main::redraw` instead).
pub fn draw_review(
    qtfb: &mut Qtfb,
    renderer: &Renderer,
    card: &ReviewCard,
    phase: Phase,
    counts: Counts,
    status: &str,
    eraser: bool,
) {
    let html = match phase {
        Phase::Question => &card.question_html,
        Phase::Answer => &card.answer_html,
    };
    let window = renderer.render_rgba(html, WIDTH as u32, CARD_H as u32);
    let fb = compose_review(
        renderer,
        &window,
        card,
        phase,
        counts,
        status,
        eraser,
        MenuState::Closed,
        false,
    );
    qtfb.blit_rgba(&fb, WIDTH, HEIGHT, 0, 0);
    let _ = qtfb.set_refresh_mode(REFRESH_MODE_CONTENT);
    let _ = qtfb.update_full();
}

/// The Review screen with no due card ("All done" / an empty deck): the top menu
/// bar (so Home / Sync / Exit still work) + a centred message filling the rest.
pub fn compose_review_empty(
    renderer: &Renderer,
    counts: Counts,
    status: &str,
    title: &str,
    body: &str,
) -> Vec<u8> {
    let mut fb = vec![255u8; WIDTH * HEIGHT * 4];
    let area_h = HEIGHT - TOP_H;
    let html = format!(
        "<!DOCTYPE html><html><head><style>\
         body{{margin:0;padding:0 120px;box-sizing:border-box;font-family:sans-serif;\
         color:#111;background:#fff;height:{ah}px;display:flex;flex-direction:column;\
         align-items:center;justify-content:center;text-align:center;}}\
         .t{{font-size:60px;font-weight:700;margin-bottom:24px;}}\
         .b{{font-size:34px;color:#444;line-height:1.5;max-width:1100px;}}</style></head>\
         <body><div class=\"t\">{}</div><div class=\"b\">{}</div></body></html>",
        esc(title),
        esc(body),
        ah = area_h
    );
    let area = renderer.render_rgba(&html, WIDTH as u32, area_h as u32);
    blit(&mut fb, &area, WIDTH, area_h, 0, TOP_H);
    let top = renderer.render_rgba(
        &top_bar_html(counts, None, status, false),
        WIDTH as u32,
        TOP_H as u32,
    );
    blit(&mut fb, &top, WIDTH, TOP_H, 0, 0);
    hline_black(&mut fb, TOP_H - 2, 2);
    fb
}

/// Full-screen message (congrats / errors) into a fresh RGBA buffer.
pub fn compose_message(renderer: &Renderer, title: &str, body: &str) -> Vec<u8> {
    let html = format!(
        "<!DOCTYPE html><html><head><style>\
         body{{font-family:sans-serif;margin:0;padding:0 120px;box-sizing:border-box;\
         color:#111;background:#fff;text-align:center;\
         display:flex;flex-direction:column;align-items:center;justify-content:center;height:100%;}}\
         .t{{font-size:64px;font-weight:700;margin-bottom:28px;}}\
         .b{{font-size:36px;color:#444;line-height:1.5;max-width:1100px;}}</style></head>\
         <body><div class=\"t\">{}</div><div class=\"b\">{}</div></body></html>",
        esc(title),
        // Callers pass multi-line bodies (e.g. the sync report); esc() keeps the
        // '\n's literal, so turn them into <br> AFTER escaping.
        esc(body).replace('\n', "<br>")
    );
    renderer.render_rgba(&html, WIDTH as u32, HEIGHT as u32)
}

/// Map a tap on the home screen to an action. `visible` is the collapse-filtered
/// deck list; `scroll` is the first displayed row. Geometry must match `compose_home`.
pub fn hit_test_home(x: i32, y: i32, visible: &[DeckInfo], scroll: usize) -> HomeHit {
    if x < 0 || y < 0 {
        return HomeHit::None;
    }
    let (x, y) = (x as usize, y as usize);
    if y < TOP_H {
        let mut edge = WIDTH;
        edge -= EXIT_W;
        if x >= edge {
            return HomeHit::Exit;
        }
        edge -= SYNC_W;
        if x >= edge {
            return HomeHit::Sync;
        }
        edge -= SCROLL_BTN_W;
        if x >= edge {
            return HomeHit::ScrollDown;
        }
        edge -= SCROLL_BTN_W;
        if x >= edge {
            return HomeHit::ScrollUp;
        }
        return HomeHit::None;
    }
    let idx = scroll + (y - TOP_H) / HOME_ROW_H;
    let Some(d) = visible.get(idx) else {
        return HomeHit::None;
    };
    // Chevron zone (parents only) toggles collapse; the rest reviews the deck.
    if d.has_children {
        let indent = HOME_PAD_L + d.level as usize * HOME_INDENT;
        if x >= indent && x < indent + HOME_CHEV_W {
            return HomeHit::Toggle(idx);
        }
    }
    HomeHit::Deck(idx)
}

/// Home screen: header (title + streak + scroll + Sync/Exit) over a collapsible,
/// paged deck tree. Renders `visible[scroll ..]` for one page.
pub fn compose_home(
    renderer: &Renderer,
    visible: &[DeckInfo],
    scroll: usize,
    stats: Stats,
) -> Vec<u8> {
    let chip = |v: u32, color: &str| {
        if v == 0 {
            "<span class=\"z\">0</span>".to_string()
        } else {
            format!("<span class=\"{color}\">{v}</span>")
        }
    };
    let fit = home_page_rows();
    let end = (scroll + fit).min(visible.len());
    let mut rows = String::new();
    for d in visible.get(scroll..end).unwrap_or(&[]) {
        let cls = if d.name == "uni" && d.level == 0 {
            "row uni"
        } else {
            "row"
        };
        let pad = HOME_PAD_L + d.level as usize * HOME_INDENT;
        let chev = if !d.has_children {
            ""
        } else if d.collapsed {
            "&#9656;" // ▸
        } else {
            "&#9662;" // ▾
        };
        rows.push_str(&format!(
            "<div class=\"{cls}\" style=\"padding-left:{pad}px;\">\
             <div class=\"chev\">{chev}</div><div class=\"nm\">{name}</div>\
             <div class=\"chips\">{n}{l}{r}</div></div>",
            name = esc(&d.name),
            n = chip(d.new, "n"),
            l = chip(d.learn, "l"),
            r = chip(d.review, "r"),
        ));
    }
    let more = if visible.len() > fit {
        format!(
            "<div class=\"pos\">{}\u{2013}{} / {}</div>",
            (scroll + 1).min(visible.len().max(1)),
            end,
            visible.len()
        )
    } else {
        String::new()
    };
    let html = format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><style>\
         html,body{{margin:0;padding:0;background:#fff;color:#111;font-family:sans-serif;}}\
         .top{{height:{top}px;display:flex;align-items:center;box-sizing:border-box;\
         border-bottom:3px solid #000;}}\
         .title{{font-size:44px;font-weight:800;padding-left:36px;}}\
         .pos{{font-size:26px;color:#888;padding-left:20px;}} .sp{{flex:1;}}\
         .streak{{font-size:30px;color:#555;padding-right:22px;white-space:nowrap;}}\
         .btn{{font-size:34px;font-weight:700;color:#fff;height:{top}px;display:flex;\
         align-items:center;justify-content:center;border-left:2px solid #fff;box-sizing:border-box;}}\
         .up,.dn{{width:{scr}px;background:#607d8b;}}\
         .sync{{width:{sync}px;background:#455a64;}} .exit{{width:{exit}px;background:#333;}}\
         .row{{height:{rowh}px;display:flex;align-items:center;box-sizing:border-box;\
         border-bottom:1px solid #dddddd;padding-right:40px;}}\
         .row.uni{{background:#eef4ff;}}\
         .chev{{width:{chevw}px;font-size:34px;color:#888;flex:none;}}\
         .nm{{flex:1;font-size:40px;font-weight:600;overflow:hidden;white-space:nowrap;\
         text-overflow:ellipsis;}}\
         .row.uni .nm{{color:#1565c0;font-weight:800;}}\
         .chips{{font-size:32px;font-weight:700;white-space:nowrap;}}\
         .chips span{{margin-left:32px;display:inline-block;min-width:1.1em;text-align:right;}}\
         .n{{color:#1565c0;}} .l{{color:#c62828;}} .r{{color:#2e7d32;}} .z{{color:#c8c8c8;}}</style></head>\
         <body><div class=\"top\"><div class=\"title\">Decks</div>{more}<div class=\"sp\"></div>\
         <div class=\"streak\">&#128293; {streak} day streak &#183; {today} today</div>\
         <div class=\"btn up\">&#9650;</div><div class=\"btn dn\">&#9660;</div>\
         <div class=\"btn sync\">Sync</div><div class=\"btn exit\">Exit</div></div>\
         {rows}</body></html>",
        top = TOP_H,
        scr = SCROLL_BTN_W,
        sync = SYNC_W,
        exit = EXIT_W,
        rowh = HOME_ROW_H,
        chevw = HOME_CHEV_W,
        more = more,
        streak = stats.streak,
        today = stats.reviewed_today,
        rows = rows,
    );
    renderer.render_rgba(&html, WIDTH as u32, HEIGHT as u32)
}

/// Composite + push the home screen.
pub fn draw_home(
    qtfb: &mut Qtfb,
    renderer: &Renderer,
    visible: &[DeckInfo],
    scroll: usize,
    stats: Stats,
) {
    let fb = compose_home(renderer, visible, scroll, stats);
    qtfb.blit_rgba(&fb, WIDTH, HEIGHT, 0, 0);
    let _ = qtfb.set_refresh_mode(REFRESH_MODE_CONTENT);
    let _ = qtfb.update_full();
}

/// Full-screen message (congrats / errors).
pub fn draw_message(qtfb: &mut Qtfb, renderer: &Renderer, title: &str, body: &str) {
    let fb = compose_message(renderer, title, body);
    qtfb.blit_rgba(&fb, WIDTH, HEIGHT, 0, 0);
    let _ = qtfb.set_refresh_mode(REFRESH_MODE_CONTENT);
    let _ = qtfb.update_full();
}

/// Alpha-composite an RGBA `src` (`sw`×`sh`) into RGBA `dst` (WIDTH-wide) at
/// (`x`,`y`) over the existing (white) destination. blitz emits transparent /
/// partially-transparent pixels for unstyled backgrounds and anti-aliased glyph
/// edges; compositing over white keeps backgrounds white and text edges clean
/// (a straight copy would turn transparency black and halo the text).
fn blit(dst: &mut [u8], src: &[u8], sw: usize, sh: usize, x: usize, y: usize) {
    for row in 0..sh {
        let py = y + row;
        if py >= HEIGHT {
            break;
        }
        for col in 0..sw {
            let px = x + col;
            if px >= WIDTH {
                break;
            }
            let si = (row * sw + col) * 4;
            let di = (py * WIDTH + px) * 4;
            let a = src[si + 3] as u32;
            if a == 0 {
                continue; // keep dst (white)
            }
            if a == 255 {
                dst[di] = src[si];
                dst[di + 1] = src[si + 1];
                dst[di + 2] = src[si + 2];
            } else {
                let ia = 255 - a;
                for k in 0..3 {
                    dst[di + k] = ((src[si + k] as u32 * a + dst[di + k] as u32 * ia) / 255) as u8;
                }
            }
            dst[di + 3] = 255;
        }
    }
}

fn hline_black(dst: &mut [u8], y: usize, thickness: usize) {
    for yy in y..(y + thickness).min(HEIGHT) {
        for x in 0..WIDTH {
            let di = (yy * WIDTH + x) * 4;
            dst[di] = 0;
            dst[di + 1] = 0;
            dst[di + 2] = 0;
            dst[di + 3] = 255;
        }
    }
}

fn top_bar_html(counts: Counts, kind: Option<CardKind>, status: &str, eraser: bool) -> String {
    // Eraser button reflects its toggle state (inverted when active).
    let (erase_bg, erase_fg) = if eraser {
        ("#111", "#fff")
    } else {
        ("#e8e8e8", "#333")
    };
    // Underline the count that matches the current card (new/learning/review).
    let cls = |k: CardKind, base: &str| {
        if kind == Some(k) {
            format!("{base} und")
        } else {
            base.to_string()
        }
    };
    let (nc, lc, rc) = (
        cls(CardKind::New, "n"),
        cls(CardKind::Learning, "l"),
        cls(CardKind::Review, "r"),
    );
    format!(
        "<!DOCTYPE html><html><head><style>\
         body{{font-family:sans-serif;margin:0;padding:0;height:{top}px;color:#111;background:#fff;\
         display:flex;align-items:center;}}\
         .counts{{flex:1;display:flex;align-items:center;gap:26px;\
         font-size:42px;padding-left:32px;}}\
         .n{{color:#1565c0;font-weight:700;}} .l{{color:#c62828;font-weight:700;}} \
         .r{{color:#2e7d32;font-weight:700;}} \
         .und{{border-bottom:7px solid;}}\
         .status{{font-size:24px;color:#666;padding-right:20px;\
         overflow:hidden;text-overflow:ellipsis;max-width:560px;}}\
         .btn{{font-size:30px;font-weight:700;text-align:center;color:#fff;\
         height:{top}px;display:flex;align-items:center;justify-content:center;\
         border-left:2px solid #fff;}}\
         .home{{width:{home}px;background:#37474f;}}\
         .undo{{width:{undo}px;background:#607d8b;}}\
         .erase{{width:{erase}px;background:{erase_bg};color:{erase_fg};}}\
         .clear{{width:{clear}px;background:#8d6e63;}}\
         .sync{{width:{sync}px;background:#455a64;}} .exit{{width:{exit}px;background:#333;}}</style></head>\
         <body><div class=\"counts\"><span class=\"{nc}\">{new}</span>\
         <span class=\"{lc}\">{learn}</span><span class=\"{rc}\">{rev}</span></div>\
         <div class=\"status\">{status}</div>\
         <div class=\"btn home\">Home</div>\
         <div class=\"btn undo\">Undo</div>\
         <div class=\"btn erase\">Eraser</div>\
         <div class=\"btn clear\">Clear</div>\
         <div class=\"btn sync\">Sync</div><div class=\"btn exit\">Exit</div></body></html>",
        top = TOP_H,
        home = HOME_W,
        undo = UNDO_W,
        erase = ERASE_W,
        clear = CLEAR_W,
        sync = SYNC_W,
        exit = EXIT_W,
        new = counts.new,
        learn = counts.learning,
        rev = counts.review,
        status = esc(status),
    )
}


fn bottom_bar_html(card: &ReviewCard, phase: Phase) -> String {
    let head = "<!DOCTYPE html><html><head><style>\
        body{font-family:sans-serif;margin:0;padding:0;height:100%;background:#fff;}\
        .row{display:flex;height:100%;}\
        .cell{flex:1;display:flex;flex-direction:column;align-items:center;justify-content:center;\
        border-left:2px solid #fff;}\
        .name{font-size:40px;font-weight:700;color:#fff;}\
        .iv{font-size:28px;color:#eee;margin-top:8px;}\
        .show{display:flex;align-items:center;justify-content:center;height:150px;\
        font-size:48px;font-weight:700;color:#fff;background:#1565c0;}</style></head><body>";

    match phase {
        Phase::Question => {
            format!("{head}<div class=\"show\">Show Answer</div></body></html>")
        }
        Phase::Answer => {
            let cells = [
                ("Again", "#c62828", &card.button_labels[0]),
                ("Hard", "#ef6c00", &card.button_labels[1]),
                ("Good", "#2e7d32", &card.button_labels[2]),
                ("Easy", "#1565c0", &card.button_labels[3]),
            ];
            let mut s = format!("{head}<div class=\"row\">");
            for (name, color, iv) in cells {
                s.push_str(&format!(
                    "<div class=\"cell\" style=\"background:{color};\">\
                     <div class=\"name\">{name}</div><div class=\"iv\">{}</div></div>",
                    esc(iv)
                ));
            }
            s.push_str("</div></body></html>");
            s
        }
    }
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
