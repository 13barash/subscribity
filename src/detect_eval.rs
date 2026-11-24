//! Developer harness (ignored test): runs the detector over a folder written by `--scan-dump`
//! (`<uid>.eml` + `index.tsv`) and prints every charge and the grouped candidates.
//! `SUBSCRIBITY_EVAL_DIR=<dump dir> cargo test --release real_mail_report -- --ignored --nocapture`
//! Optional `SUBSCRIBITY_EVAL_TEXT=<dir>` writes each parsed mail as `<uid>.txt` (for reading).
//! No paths to real data live in the source.
use super::*;
use crate::mime::{parse, split_eml, RawMail};

fn period_s(e: u32, u: u8) -> String {
    format!("{e}{}", ["?", "w", "m", "y"].get(u as usize).unwrap_or(&"?"))
}

#[test]
#[ignore]
fn real_mail_report() {
    let Some(dir) = std::env::var_os("SUBSCRIBITY_EVAL_DIR") else { return };
    let dir = std::path::PathBuf::from(dir);
    let text_dir = std::env::var_os("SUBSCRIBITY_EVAL_TEXT").map(std::path::PathBuf::from);
    let index = std::fs::read_to_string(dir.join("index.tsv")).expect("index.tsv");
    let mut charges = Vec::new();
    let mut out = String::new();
    let mut max_day = 0;
    for line in index.lines().skip(1) {
        let f: Vec<&str> = line.split('\t').collect();
        let (Some(uid), Some(d)) = (f.first(), f.get(1).and_then(|d| data::parse_iso(d))) else { continue };
        let Ok(eml) = std::fs::read(dir.join(format!("{uid}.eml"))) else { continue };
        max_day = max_day.max(d);
        let (header, body) = split_eml(&eml);
        let p = parse(&RawMail { header, body });
        if let Some(td) = &text_dir {
            let _ = std::fs::write(td.join(format!("{uid}.txt")), format!("FROM {} <{}>\nSUBJ {}\n\n{}", p.from_name, p.from_addr, p.subject, p.text));
        }
        let cs = charges_from_mail(&p, d);
        if cs.is_empty() {
            out.push_str(&format!("- {uid}\t{}\t{}\n", data::iso(d), p.from_addr.rsplit('@').next().unwrap_or("")));
        }
        for c in cs {
            out.push_str(&format!(
                "M {uid}\t{}\t{}\t{}\t{} {}\t{:?}\tpm={}\ttrial={}\tflags={:07b}\tren={}\n",
                data::iso(d), c.key, c.name, c.price, c.cur, c.period, c.method, c.trial, c.score_parts,
                c.renews.map_or("-".into(), data::iso)
            ));
            charges.push(c);
        }
    }
    let today = std::env::var("SUBSCRIBITY_EVAL_TODAY").ok().and_then(|s| data::parse_iso(&s)).unwrap_or(max_day + 1);
    out.push_str("\ncandidate | price cur | period | first | last | next | method | category | score | count | trial | site | apple\n");
    for c in group(&charges, today) {
        out.push_str(&format!(
            "C {} | {} {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {}\n",
            c.name, c.price, c.cur, period_s(c.every, c.unit), data::iso(c.first), data::iso(c.last), data::iso(c.next),
            c.method, c.category, c.score, c.count, c.trial, c.site, c.apple
        ));
    }
    println!("{out}");
}
