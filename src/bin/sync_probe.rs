// Headless sync probe — runs `Backend::sync` against a collection and prints the
// full outcome (success report or the real error, `{e:#}` + `{e:?}`), without
// QTFB/touch. Point ANKIMARKABLE_COL at a COPY of the live collection to diagnose
// sync failures over SSH while the app keeps running (anki takes an exclusive
// lock on open, so never aim this at the file the app holds).
//
// Usage on device:
//   ANKIMARKABLE_COL=/home/root/col_copy.anki2 ./sync_probe
// Creds: /home/root/.ankimarkable/ankiweb.txt (line 1 = email/user, line 2 = pass),
// same file main's do_sync reads.

use ankimarkable::backend::Backend;

fn main() -> anyhow::Result<()> {
    let col = std::env::var("ANKIMARKABLE_COL")
        .unwrap_or_else(|_| "/home/root/.ankimarkable/collection.anki2".to_string());
    println!("collection: {col}");

    let creds = std::fs::read_to_string("/home/root/.ankimarkable/ankiweb.txt")?;
    let mut lines = creds.lines();
    let (Some(user), Some(pass)) = (lines.next(), lines.next()) else {
        anyhow::bail!("ankiweb.txt needs user + pass lines");
    };

    let mut be = Backend::open(&col)?;
    match be.sync(user.trim(), pass.trim()) {
        Ok(s) => println!("sync OK: {s}"),
        Err(e) => println!("sync FAILED: {e:#}  ||  debug: {e:?}"),
    }
    Ok(())
}
