// Headless integration test — renders the COMPOSITED review screen (top bar +
// card + grade buttons) to PNG using the real Backend + Renderer + ui compositor,
// without QTFB/touch. Lets us verify the full UI on-device by eyeballing the PNGs.
//
// Usage on device:  ./spike_screen   (reads /home/root/.ankimarkable/collection.anki2)

use ankimarkable::backend::Backend;
use ankimarkable::render::Renderer;
use ankimarkable::ui::{self, Phase};

/// Percent-encode a media filename for use in an `<img src>` (keep unreserved).
fn pct_encode(s: &str) -> String {
    let mut out = String::new();
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn save_png(path: &str, rgba: &[u8], w: usize, h: usize) -> anyhow::Result<()> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w as u32, h as u32);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        let mut wr = enc.write_header()?;
        wr.write_image_data(rgba)?;
    }
    std::fs::write(path, &out)?;
    println!("wrote {path} ({} bytes)", out.len());
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let col = std::env::var("ANKIMARKABLE_COL")
        .unwrap_or_else(|_| "/home/root/.ankimarkable/collection.anki2".to_string());
    // Read stats BEFORE opening (anki takes an exclusive lock on open).
    let stats = ankimarkable::backend::read_stats(&col);
    let mut be = Backend::open(&col)?;

    // Opt-in headless AnkiWeb sync (ANKIMARKABLE_SYNC=1): mirrors main::do_sync so
    // the full creds→sync→uni-arrival→scope chain can be verified over SSH without a
    // live icon tap. Reads /home/root/.ankimarkable/ankiweb.txt (line1 user, line2 pass).
    if std::env::var("ANKIMARKABLE_SYNC").as_deref() == Ok("1") {
        let creds = std::fs::read_to_string("/home/root/.ankimarkable/ankiweb.txt")?;
        let mut l = creds.lines();
        if let (Some(u), Some(p)) = (l.next(), l.next()) {
            match be.sync(u.trim(), p.trim()) {
                Ok(r) => println!("sync: {}", r.headline),
                Err(e) => println!("sync FAILED: {e:#}  ||  debug: {e:?}"),
            }
        } else {
            println!("sync: ankiweb.txt missing user/pass lines");
        }
    }

    // Match the app: scope to the ANKIMARKABLE_DECK deck (default "uni") if present.
    let deck = std::env::var("ANKIMARKABLE_DECK").unwrap_or_else(|_| "uni".to_string());
    let scoped = be.set_deck_scope(&deck).unwrap_or(false);
    println!("deck scope '{deck}': {}", if scoped { "applied" } else { "not found" });
    let counts = be.counts()?;
    println!("counts after scope: new={} learning={} review={}", counts.new, counts.learning, counts.review);
    println!(
        "counts new={} learning={} review={}",
        counts.new, counts.learning, counts.review
    );
    let media_dir = std::path::Path::new(&col).with_extension("media");
    let r = Renderer::new().with_media_dir(media_dir.clone());

    // Glyph-fallback probe: emoji + symbols that aren't in Noto Sans/Mono.
    {
        let html = "<!DOCTYPE html><html><body style=\"background:#fff;margin:0;padding:30px;\
            font-family:sans-serif;font-size:56px\">\
            thumbs &#128077; &#128078; star &#9733; check &#10003; cross &#10007; \
            arrow &#8594; bullet &#9642; sun &#9728;</body></html>";
        let buf = r.render_rgba(html, ui::WIDTH as u32, 400);
        save_png("/home/root/screen_glyph.png", &buf, ui::WIDTH, 400)?;
    }

    // Image-provider probe: render the first image we find in media (percent-encode).
    if let Ok(rd) = std::fs::read_dir(&media_dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let lower = name.to_lowercase();
            let is_img = [".jpg", ".jpeg", ".png", ".gif"]
                .iter()
                .any(|ext| lower.ends_with(ext));
            if is_img {
                let enc = pct_encode(&name);
                let html = format!(
                    "<!DOCTYPE html><html><body style=\"background:#fff;margin:0;padding:20px\">\
                     <div style=\"font-family:sans-serif;font-size:32px\">img: {name}</div>\
                     <img src=\"{enc}\" style=\"max-width:1400px\"></body></html>"
                );
                let buf = r.render_rgba(&html, ui::WIDTH as u32, 1000);
                save_png("/home/root/screen_img.png", &buf, ui::WIDTH, 1000)?;
                println!("img probe: {name}");
                break;
            }
        }
    }

    // Home screen (collapsible deck tree, rendered fully expanded from row 0).
    {
        let decks = be.deck_infos()?;
        println!("home: {} decks, streak={} today={}", decks.len(), stats.streak, stats.reviewed_today);
        let home = ui::compose_home(&r, &decks, 0, stats);
        save_png("/home/root/screen_home.png", &home, ui::WIDTH, ui::HEIGHT)?;
    }

    // Optional: render a specific card (e.g. a "Pretty with Note" card) to check the
    // bigger sizing + the revealed Note. Set ANKIMARKABLE_CID=<card id>.
    if let Ok(cids) = std::env::var("ANKIMARKABLE_CID") {
        if let Ok(cid) = cids.parse::<i64>() {
            let (_q, a) = be.card_html(cid)?;
            let buf = r.render_rgba(&a, ui::WIDTH as u32, ui::CARD_H as u32);
            save_png("/home/root/screen_note.png", &buf, ui::WIDTH, ui::CARD_H)?;
            println!("rendered card {cid} answer → screen_note.png");
        }
    }

    // Scroll/zoom verification (ANKIMARKABLE_SCROLL_CID=<cid>): drive the live
    // CardView headlessly — paint the answer at several scroll offsets and one
    // zoom step, so the whole scroll/zoom path is verifiable without touch input.
    if let Ok(cids) = std::env::var("ANKIMARKABLE_SCROLL_CID") {
        if let Ok(cid) = cids.parse::<i64>() {
            let (_q, a) = be.card_html(cid)?;
            let mut v = r.build_card_view(&a, ui::WIDTH as u32, ui::CARD_H as u32);
            println!(
                "scroll test: content_h={:.0} max_scroll={:.0}",
                v.content_h(),
                v.max_scroll()
            );
            for (i, off) in [0.0f64, 800.0, 1600.0].iter().enumerate() {
                let cur = v.scroll_phys();
                v.scroll_by(off - cur);
                let mut w = v.paint_window();
                ui::draw_scroll_indicator(&mut w, v.scroll_phys(), v.max_scroll(), v.content_h());
                save_png(&format!("/home/root/scroll_{i}.png"), &w, ui::WIDTH, ui::CARD_H)?;
                println!("scroll_{i} at phys {:.0}", v.scroll_phys());
            }
            v.set_zoom_step(1);
            let w = v.paint_window();
            save_png("/home/root/scroll_zoom.png", &w, ui::WIDTH, ui::CARD_H)?;
            println!("zoomed → zoom={} content_h={:.0}", v.zoom(), v.content_h());
        }
    }

    match be.next_card()? {
        Some(card) => {
            let qwin = r.render_rgba(&card.question_html, ui::WIDTH as u32, ui::CARD_H as u32);
            let q = ui::compose_review(&r, &qwin, &card, Phase::Question, counts, "ready", false, false, false);
            save_png("/home/root/screen_q.png", &q, ui::WIDTH, ui::HEIGHT)?;
            let awin = r.render_rgba(&card.answer_html, ui::WIDTH as u32, ui::CARD_H as u32);
            let a = ui::compose_review(&r, &awin, &card, Phase::Answer, counts, "ready", false, false, false);
            save_png("/home/root/screen_a.png", &a, ui::WIDTH, ui::HEIGHT)?;
            println!("buttons: {:?}", card.button_labels);
        }
        None => {
            let m = ui::compose_message(&r, "All done", "No cards due");
            save_png("/home/root/screen_m.png", &m, ui::WIDTH, ui::HEIGHT)?;
        }
    }
    println!("SPIKE SCREEN OK");
    Ok(())
}
