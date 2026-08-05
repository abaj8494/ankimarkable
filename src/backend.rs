//! rslib wrapper — the Anki backend the app drives.
//!
//! Owns the open `Collection` and exposes exactly what the review UI needs:
//! due counts, the next due card (already rendered to full card HTML+CSS via
//! Anki's own template engine), grading (real FSRS scheduler), and AnkiWeb sync.
//! Card *rendering to pixels* lives in `render.rs`; this module only produces the
//! HTML/CSS strings that Anki itself would feed a webview.

use anki::card::{CardId, CardQueueNumber};
use anki::collection::{Collection, CollectionBuilder};
use anki::decks::DeckId;
use anki::scheduler::answering::{CardAnswer, Rating};
use anki::scheduler::states::SchedulingStates;
use anki::search::SortMode;
use anki_proto::scheduler::bury_or_suspend_cards_request::Mode as BuryMode;
use anki::sync::collection::normal::SyncActionRequired;
use anki::sync::login::sync_login;
use anki::sync::media::progress::MediaSyncProgress;
use anki::template::RenderedNode;
use anki::timestamp::{TimestampMillis, TimestampSecs};
use anyhow::Result;
use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

/// The four Anki grades, in button order.
#[derive(Clone, Copy, Debug)]
pub enum Grade {
    Again,
    Hard,
    Good,
    Easy,
}

/// Due counts for the current queue (shown as chrome).
#[derive(Clone, Copy, Debug, Default)]
pub struct Counts {
    pub new: usize,
    pub learning: usize,
    pub review: usize,
}

/// A deck row for the home screen's collapsible tree. `name` is the leaf component
/// (indent by `level`); counts include subdecks (Anki's deck-list numbers).
/// `collapsed` starts from Anki's stored state and is then toggled at runtime.
#[derive(Clone, Debug)]
pub struct DeckInfo {
    pub id: DeckId,
    pub name: String,
    pub level: u32,
    pub has_children: bool,
    pub collapsed: bool,
    pub new: u32,
    pub learn: u32,
    pub review: u32,
}

/// Review stats for the home screen.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub streak: u32,
    pub reviewed_today: u32,
}

/// What a sync actually did, for the post-sync message screen. `headline` is a
/// short status ("synced" / "up to date" / …) that also fits the top-bar status
/// div; the Option lines are pre-localized detail strings from rslib's progress
/// (e.g. "Added/modified: ↑3 ↓12") — None when a stage didn't run or reported
/// nothing.
#[derive(Default)]
pub struct SyncReport {
    pub headline: String,
    pub added: Option<String>,
    pub removed: Option<String>,
    pub media: Option<String>,
    pub server_message: Option<String>,
}

/// Which queue the current card came from — drives the underline under the
/// matching top-bar count (new = blue, learning/redo = red, review = green).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CardKind {
    New,
    Learning,
    Review,
}

/// A card ready to review: full self-contained HTML for each side plus the
/// per-grade interval previews ("<1m", "1d", …). `states` is retained so the
/// chosen grade maps to the exact next CardState the scheduler computed.
pub struct ReviewCard {
    pub card_id: CardId,
    /// The card's note id (for the ⋮ menu's "Suspend note" action).
    pub note_id: i64,
    pub kind: CardKind,
    pub question_html: String,
    pub answer_html: String,
    /// [again, hard, good, easy]
    pub button_labels: [String; 4],
    states: SchedulingStates,
}

pub struct Backend {
    // `Option` so `sync` can take the collection by value for a full download
    // (which consumes + replaces the DB) and then re-open it. Always `Some`
    // outside that brief window.
    col: Option<Collection>,
    col_path: String,
    // Turns `[latex]`/`[$]` SVGs + MathJax `\(...\)` in card HTML into `<img>` PNGs.
    math: crate::mathrender::MathRenderer,
}

impl Backend {
    /// Open (or create) the collection at `col_path`, wiring desktop-style media
    /// paths (`foo.media/`, `foo.mdb`) so card `<img>` and media sync resolve.
    pub fn open(col_path: &str) -> Result<Self> {
        let col = CollectionBuilder::new(col_path)
            .with_desktop_media_paths()
            .build()?;
        let media_dir = std::path::Path::new(col_path).with_extension("media");
        Ok(Self {
            col: Some(col),
            col_path: col_path.to_string(),
            math: crate::mathrender::MathRenderer::new(media_dir),
        })
    }

    /// The open collection. In normal flow it is always present; `sync` restores
    /// it before returning even on the full-download path.
    fn col(&mut self) -> &mut Collection {
        self.col.as_mut().expect("collection is open")
    }

    /// Re-open the collection from disk (after a full download replaced the file).
    fn reopen(&mut self) -> Result<()> {
        self.col = Some(
            CollectionBuilder::new(&self.col_path)
                .with_desktop_media_paths()
                .build()?,
        );
        Ok(())
    }

    /// Scope all reviewing to a single deck (and, automatically, its subdecks) by
    /// human name — e.g. `"uni"` or `"uni::algorithms"`. rslib builds the queue and
    /// due counts from the *current* deck, so setting it here filters everything
    /// downstream (`counts`, `next_card`) with no further changes. Non-destructive:
    /// other decks stay in the collection, just never surfaced.
    ///
    /// Returns `Ok(true)` if the deck exists and was selected, `Ok(false)` if no
    /// such deck is present yet (e.g. before the first AnkiWeb sync creates it) —
    /// the caller can surface a "sync to fetch it" hint and keep running.
    pub fn set_deck_scope(&mut self, human_name: &str) -> Result<bool> {
        match self.col().get_deck_id(human_name)? {
            Some(did) => {
                self.col().set_current_deck(did)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Scope reviewing to a specific deck id (from the home-screen deck list).
    pub fn set_deck_scope_id(&mut self, did: DeckId) -> Result<()> {
        self.col().set_current_deck(did)?;
        Ok(())
    }

    /// The full deck tree flattened depth-first for the home screen: each deck with
    /// its `level`, `has_children`, Anki `collapsed` state, and due counts. Top-level
    /// decks are `uni`-first then alphabetical; each subtree stays contiguous and its
    /// children are sorted alphabetically. Empty `Default` at the top is dropped.
    pub fn deck_infos(&mut self) -> Result<Vec<DeckInfo>> {
        let tree = self.col().deck_tree(Some(TimestampSecs::now()))?;
        let mut top: Vec<_> = tree.children.iter().collect();
        top.sort_by(|a, b| {
            (a.name != "uni", a.name.to_lowercase())
                .cmp(&(b.name != "uni", b.name.to_lowercase()))
        });
        // Pre-order DFS via a stack (node ref + level). Push siblings reversed so
        // pop() yields them in order; keeps each subtree contiguous. The proto node
        // type stays inferred (`_`) — no need to name `anki_proto`.
        let mut out = Vec::new();
        let mut stack = Vec::new();
        for node in top.into_iter().rev() {
            if node.name == "Default"
                && node.new_count + node.learn_count + node.review_count == 0
                && node.children.is_empty()
            {
                continue;
            }
            stack.push((node, 0u32));
        }
        while let Some((node, level)) = stack.pop() {
            out.push(DeckInfo {
                id: DeckId(node.deck_id),
                name: node.name.clone(),
                level,
                has_children: !node.children.is_empty(),
                collapsed: node.collapsed,
                new: node.new_count,
                learn: node.learn_count,
                review: node.review_count,
            });
            let mut kids: Vec<_> = node.children.iter().collect();
            kids.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
            for kid in kids.into_iter().rev() {
                stack.push((kid, level + 1));
            }
        }
        Ok(out)
    }


    pub fn counts(&mut self) -> Result<Counts> {
        // fetch_limit 0 → no cards materialised, but counts still come from the queue.
        let q = self.col().get_queued_cards(0, false)?;
        Ok(Counts {
            new: q.new_count,
            learning: q.learning_count,
            review: q.review_count,
        })
    }

    /// The next due card, rendered to full card HTML, or None when the queue is
    /// exhausted (congrats screen).
    pub fn next_card(&mut self) -> Result<Option<ReviewCard>> {
        let queued = self.col().get_queued_cards(1, false)?;
        let Some(qc) = queued.cards.into_iter().next() else {
            return Ok(None);
        };
        let cid = qc.card.id();
        let kind = match qc.card.queue_number() {
            CardQueueNumber::New => CardKind::New,
            CardQueueNumber::Learning => CardKind::Learning,
            // Review or (shouldn't happen for a queued card) Invalid → review.
            _ => CardKind::Review,
        };

        let out = self.col().render_existing_card(cid, false, false)?;
        let q_html = self.math.preprocess(&nodes_to_html(&out.qnodes));
        let a_html = self.math.preprocess(&nodes_to_html(&out.anodes));
        let question = wrap_card(&out.css, &q_html);
        let answer = wrap_card(&out.css, &a_html);

        let labels = self.col().describe_next_states(&qc.states)?;
        let button_labels = [
            labels.first().cloned().unwrap_or_default(),
            labels.get(1).cloned().unwrap_or_default(),
            labels.get(2).cloned().unwrap_or_default(),
            labels.get(3).cloned().unwrap_or_default(),
        ];

        Ok(Some(ReviewCard {
            card_id: cid,
            note_id: qc.card.note_id().0,
            kind,
            question_html: question,
            answer_html: answer,
            button_labels,
            states: qc.states,
        }))
    }

    /// Render a specific card's (question, answer) HTML by card id — used by the
    /// headless screen test to render a chosen notetype (e.g. "Pretty with Note").
    pub fn card_html(&mut self, cid: i64) -> Result<(String, String)> {
        let out = self.col().render_existing_card(CardId(cid), false, false)?;
        let q = self.math.preprocess(&nodes_to_html(&out.qnodes));
        let a = self.math.preprocess(&nodes_to_html(&out.anodes));
        Ok((wrap_card(&out.css, &q), wrap_card(&out.css, &a)))
    }

    /// Undo the last undoable operation (e.g. the previous grade) so the card comes
    /// back. Returns true if something was undone, false if the undo queue is empty.
    /// Bury the current card (⋮ menu → "Bury card"): hides it until tomorrow.
    pub fn bury_card(&mut self, cid: CardId) -> Result<()> {
        self.col().bury_or_suspend_cards(&[cid], BuryMode::BuryUser)?;
        Ok(())
    }

    /// Suspend every card of a note (⋮ menu → "Suspend note"): removes them from
    /// review until manually unsuspended.
    pub fn suspend_note(&mut self, note_id: i64) -> Result<()> {
        let cids = self.col().search_cards(&format!("nid:{note_id}"), SortMode::NoOrder)?;
        if !cids.is_empty() {
            self.col().bury_or_suspend_cards(&cids, BuryMode::Suspend)?;
        }
        Ok(())
    }

    pub fn undo(&mut self) -> Result<bool> {
        Ok(self.col().undo().is_ok())
    }

    /// Grade a card — writes the new scheduling state to the collection DB.
    pub fn answer(&mut self, card: &ReviewCard, grade: Grade) -> Result<()> {
        let (rating, new_state) = match grade {
            Grade::Again => (Rating::Again, card.states.again),
            Grade::Hard => (Rating::Hard, card.states.hard),
            Grade::Good => (Rating::Good, card.states.good),
            Grade::Easy => (Rating::Easy, card.states.easy),
        };
        let mut answer = CardAnswer {
            card_id: card.card_id,
            current_state: card.states.current,
            new_state,
            rating,
            answered_at: TimestampMillis::now(),
            milliseconds_taken: 0,
            custom_data: None,
            from_queue: true,
        };
        self.col().answer_card(&mut answer)?;
        Ok(())
    }

    /// Sync with AnkiWeb: login → collection sync → media sync.
    ///
    /// If AnkiWeb requires a full sync, we auto-resolve by **downloading** — this is
    /// a review-only client whose on-device collection is disposable, so pulling
    /// AnkiWeb's authoritative copy is always the right (and safe) resolution. We
    /// never auto-*upload* (that could clobber AnkiWeb), so an upload-only full sync
    /// is surfaced for the user to resolve on desktop. This is what lets a stale or
    /// freshly-provisioned device fetch the `uni` deck on first sync.
    pub fn sync(&mut self, username: &str, password: &str) -> Result<SyncReport> {
        use anki::services::CollectionService; // for latest_progress()
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        // Matches anki's default web client. `.no_proxy()` because reqwest honours
        // HTTP(S)_PROXY env vars by default — a proxy inherited from the launching
        // shell/appload env would silently reroute AnkiWeb traffic and fail.
        let client = reqwest::Client::builder().http1_only().no_proxy().build()?;
        let auth = rt
            .block_on(sync_login(username, password, None, client.clone()))
            .map_err(describe_sync_error)?;

        // 1) Collection sync.
        let out = rt
            .block_on(self.col().normal_sync(auth.clone(), client.clone()))
            .map_err(describe_sync_error)?;
        let mut report = SyncReport::default();
        if !out.server_message.is_empty() {
            report.server_message = Some(out.server_message.clone());
        }
        // Grab the collection-sync counts NOW: rslib only exposes them through a
        // single shared progress slot, and the media sync below overwrites it.
        // The pre-localized `added`/`removed` lines ("Added/modified: ↑x ↓y") are
        // exactly what the desktop shows. On NoChanges the slot may hold some
        // other variant — the match just leaves the fields None.
        if let Ok(p) = self.col().latest_progress() {
            if let Some(anki_proto::collection::progress::Value::NormalSync(ns)) = p.value {
                report.added = Some(ns.added);
                report.removed = Some(ns.removed);
            }
        }
        let status = match out.required {
            SyncActionRequired::NoChanges => "up to date",
            SyncActionRequired::NormalSyncRequired => "synced",
            SyncActionRequired::FullSyncRequired { download_ok, .. } => {
                if download_ok {
                    // AnkiWeb shards accounts across hosts; `normal_sync` follows the
                    // redirect for its own requests and reports the sharded endpoint in
                    // `new_endpoint`. A fresh `full_download` builds a new client from
                    // `auth`, so we must point it at that endpoint or it hits the wrong
                    // host and fails with SyncError.
                    let mut dl_auth = auth.clone();
                    if let Some(ep) = out.new_endpoint.as_deref() {
                        if let Ok(url) = reqwest::Url::parse(ep) {
                            dl_auth.endpoint = Some(url);
                        }
                    }
                    // Consume the collection for the full download (it closes the DB
                    // and replaces the file wholesale). Always re-open afterwards —
                    // even on failure the handle is closed, so we must restore
                    // `self.col` before propagating any error (else later `col()`
                    // calls panic).
                    let col = self.col.take().expect("collection is open");
                    let res = rt.block_on(col.full_download(dl_auth, client.clone()));
                    self.reopen()?;
                    res.map_err(describe_sync_error)?;
                    // The normal-sync counts captured above are moot — the whole
                    // collection was replaced, not incrementally merged.
                    report.added = None;
                    report.removed = None;
                    "downloaded"
                } else {
                    report.headline = "full sync needs upload — resolve on desktop".to_string();
                    return Ok(report);
                }
            }
        };

        // 2) Media (card images/audio) — synced after the collection is settled.
        let mgr = self.col().media()?;
        let progress = self.col().new_progress_handler::<MediaSyncProgress>();
        rt.block_on(mgr.sync_media(progress, auth, client, None))
            .map_err(describe_sync_error)?;
        // The slot now holds the media stage's final (pre-localized) lines:
        // "Checked: n" / "Added: ↑x ↓y" / "Removed: ↑x ↓y" — fold into one line.
        if let Ok(p) = self.col().latest_progress() {
            if let Some(anki_proto::collection::progress::Value::MediaSync(ms)) = p.value {
                report.media = Some(format!("media: {} · {} · {}", ms.checked, ms.added, ms.removed));
            }
        }

        report.headline = status.to_string();
        Ok(report)
    }
}

/// Convert a concrete `AnkiError` into an anyhow error that actually says what
/// went wrong. snafu's derived Display on `AnkiError::NetworkError` prints just
/// the bare variant name ("NetworkError"), swallowing the inner kind + info —
/// so a DNS failure, a timeout and an auth-proxy 407 all surface identically.
/// Pull the inner fields out here, before `?` erases the type into anyhow.
fn describe_sync_error(e: anki::error::AnkiError) -> anyhow::Error {
    use anki::error::AnkiError as E;
    match &e {
        E::NetworkError { source } => anyhow::anyhow!("network {:?}: {}", source.kind, source.info),
        E::SyncError { source } => anyhow::anyhow!("sync {:?}: {}", source.kind, source.info),
        _ => anyhow::anyhow!("{e:?}"),
    }
}

/// Join Anki's rendered template nodes into an HTML string. The template engine
/// has already substituted fields and applied filters, so Text and Replacement
/// both contribute literal HTML.
fn nodes_to_html(nodes: &[RenderedNode]) -> String {
    let mut s = String::new();
    for n in nodes {
        match n {
            RenderedNode::Text { text } => s.push_str(text),
            RenderedNode::Replacement { current_text, .. } => s.push_str(current_text),
        }
    }
    s
}

/// Wrap a card side in the same shell Anki's webview uses: the notetype CSS in a
/// `<style>` block, body class `card`, content in `#qa`.
///
/// Typography: the content is nested in an `.am-pad` frame we own so a deck's own
/// CSS (which targets `.card`/`.front`/`.back`, almost never `.am-pad`) can't
/// override the page margins. Generous side padding, a capped measure centred in
/// the panel, and a comfortable line-height give the card room to breathe instead
/// of hugging the bezel. Our reset runs BEFORE the notetype CSS so decks can still
/// restyle their own content.
fn wrap_card(css: &str, body: &str) -> String {
    // Two style blocks: a baseline BEFORE the notetype CSS (so decks can restyle
    // their own content), and an override block AFTER it (so it wins). The override
    // (a) enlarges cards — many decks here use "anki-prettify", which caps itself at
    // `--card-max-width: 40em` / 16px, rendering a small column on the 1620px panel;
    // we lift the width cap and bump the fonts so cards fill the screen — and
    // (b) reveals the prettify "Note"/"Example" panes, which are `display:none` and
    // JS-toggled (blitz runs no JS), so they'd never show otherwise.
    format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><style>\
         html,body{{background:#fff;color:#000;margin:0;padding:0;}}\
         body.card{{font-size:40px;line-height:2.0;}}\
         .am-pad{{box-sizing:border-box;padding:64px 88px;max-width:1500px;\
         margin:0 auto;}}\
         .am-pad img{{max-width:100%;height:auto;}}\
         .am-pad hr{{border:none;border-top:2px solid #d0d0d0;margin:40px 0;}}\
         {css}\
         /* am-overrides — win over the notetype CSS */\
         :root{{--font-size-regular:30px!important;--font-size-small:24px!important;\
         --card-max-width:60em!important;--img-width:66%!important;}}\
         body.card{{font-size:36px!important;line-height:2.0!important;}}\
         .am-pad,#qa{{line-height:2.0!important;}}\
         .prettify-flashcard{{max-width:none!important;}}\
         .am-pad{{max-width:1520px!important;}}\
         #note-content,#example-content{{display:block!important;}}\
         #note-button,#example-button{{display:none!important;}}\
         /* Math images are TEXT, not pictures: a notetype's `img{{…}}` rules \
            (anki-prettify: display:block; margin:1em auto; max-width:50%) would \
            otherwise drop every inline \\(x\\) onto its own centred line, and cap \
            display math at half width. Desktop Anki never sees this because \
            MathJax isn't an <img>. */\
         img.am-math{{display:inline!important;margin:0 0.12em!important;\
         max-width:100%!important;border-radius:0!important;opacity:1!important;\
         filter:none!important;}}\
         img.am-math+br{{display:inline!important;}}\
         img.am-math-block{{display:block!important;margin:26px auto!important;\
         max-width:100%!important;border-radius:0!important;opacity:1!important;\
         filter:none!important;}}\
         </style></head>\
         <body class=\"card\"><div class=\"am-pad\"><div id=\"qa\">{body}</div></div></body></html>"
    )
}

/// Read the review streak + reviews-done-today from `revlog`.
///
/// MUST be called BEFORE the collection is opened by rslib: anki sets
/// `PRAGMA locking_mode=exclusive`, so once it holds the file no other connection
/// can read it. The caller computes this once at startup (nothing holds the lock
/// yet), caches it, and increments `reviewed_today` per in-session grade. Day index
/// is `floor((review_secs - crt)/86400)`; streak = consecutive days ending today or
/// (grace) yesterday. Best-effort: any error → zeroes.
pub fn read_stats(path: &str) -> Stats {
    stats_from_db(path).unwrap_or_default()
}

fn stats_from_db(path: &str) -> Result<Stats> {
    use rusqlite::{Connection, OpenFlags};
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    let crt: i64 = conn.query_row("select crt from col", [], |r| r.get(0))?;
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let today = (now_secs - crt) / 86_400;

    let mut stmt =
        conn.prepare("select distinct cast((id/1000 - ?1)/86400 as integer) from revlog where type != 4")?;
    let days: HashSet<i64> = stmt
        .query_map([crt], |r| r.get::<_, i64>(0))?
        .filter_map(|x| x.ok())
        .collect();

    let mut streak = 0u32;
    let mut d = if days.contains(&today) { today } else { today - 1 };
    while days.contains(&d) {
        streak += 1;
        d -= 1;
    }

    let reviewed_today: u32 = conn
        .query_row(
            "select count(*) from revlog where type != 4 and cast((id/1000 - ?1)/86400 as integer) = ?2",
            (crt, today),
            |r| r.get::<_, i64>(0),
        )
        .map(|c| c as u32)
        .unwrap_or(0);

    Ok(Stats {
        streak,
        reviewed_today,
    })
}
