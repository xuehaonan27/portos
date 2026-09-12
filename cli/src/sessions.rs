//! `portos sessions <root>` — what has been stored, newest first.
//!
//! Reads the driver's index rather than asking it, so it works with nothing
//! running. The format is `portos_model_api::SessionIndex`, which is where
//! the writer and this reader agree; the transcripts themselves stay in the
//! CAS and are not touched to produce this list. A running front end asks
//! `model::sessions` instead.

use portos_model_api as model;
use std::path::PathBuf;

pub fn sessions(root: &str) -> Result<(), Box<dyn std::error::Error>> {
    let index = model::SessionIndex::read(&PathBuf::from(root).join(model::DIR));
    let listed = index.by_recency();
    if listed.is_empty() {
        println!("no stored sessions");
        return Ok(());
    }
    println!(
        "{:<8} {:>6}  {:<20} {}",
        "SESSION", "TURNS", "UPDATED", "OPENING"
    );
    for (id, rec) in listed {
        println!(
            "{:<8} {:>6}  {:<20} {}",
            id,
            rec.turns,
            stamp(rec.updated_at),
            rec.title
        );
    }
    Ok(())
}

/// Unix seconds as something a person can read, without pulling in a date
/// library for one column.
fn stamp(secs: u64) -> String {
    let days = secs / 86_400;
    let (h, m) = ((secs % 86_400) / 3600, (secs % 3600) / 60);
    // 1970-01-01 plus `days`, by the civil-from-days algorithm.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}")
}
