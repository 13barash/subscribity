//! PayPal activity CSV: rows mapped by the window's JS (column aliases below) → charges.
//! Declared from detect.rs (`detect::paypal`).
use super::{first_date, norm, parse_price, period_from_gaps, Charge, AMOUNT, KNOWN, RECEIPT, RECUR};
use crate::data;
use crate::services;
use serde_json::{json, Value};
use std::collections::HashMap;

pub const MAX_ROWS: usize = 20_000;
const DATE_ERROR: &str = "Could not read the dates in this file. Set PayPal's language to English and download the activity file again.";

#[derive(Clone, Debug, Default)]
pub struct PaypalRow {
    pub date: String,
    pub name: String,
    pub typ: String,
    pub status: String,
    pub cur: String,
    pub gross: String,
}

/// Header names per field (en/de/fr/es/he), in preference order: the JS mapper takes the first
/// alias present in the header row. `gross` falls back to Net, then Amount.
pub fn paypal_column_aliases() -> Value {
    json!({
        "date": ["Date", "Datum", "Fecha", "Data", "תאריך"],
        "name": ["Name", "Nom", "Nombre", "Nome", "שם"],
        "type": ["Type", "Typ", "Tipo", "Description", "Beschreibung", "סוג", "תיאור"],
        "status": ["Status", "Statut", "État", "Etat", "Estado", "Stato", "מצב", "סטטוס"],
        "currency": ["Currency", "Währung", "Devise", "Divisa", "Moneda", "Valuta", "מטבע"],
        "gross": ["Gross", "Brutto", "Brut", "Bruto", "Lordo", "ברוטו", "Net", "Netto", "Neto", "נטו",
                  "Amount", "Betrag", "Montant", "Importe", "Importo", "סכום"],
    })
}

const DROP: [&str; 34] = [
    "conversion", "umrechnung", "conversión", "transfer", "überweisung", "virement", "withdrawal", "auszahlung",
    "retrait", "retiro", "hold", "reserve", "rückbehalt", "retenue", "retención", "refund", "rückzahlung",
    "erstattung", "remboursement", "reembolso", "authori", "autoris", "autoriz", "deposit", "einzahlung",
    "dépôt", "depósito", "המר", "העבר", "משיכ", "החזר", "זיכוי", "הפקד", "הרשא",
];
const RECURRING: [&str; 20] = [
    "subscription", "pre-approved", "preapproved", "automatic", "recurring", "abonnement", "vorab genehmigt",
    "automatische", "wiederkehrend", "suscripción", "suscripcion", "préapprouvé", "preaprobado", "automático",
    "automatico", "récurrent", "recurrente", "מנוי", "אוטומטי", "הוראת קבע",
];
const DONE: [&str; 9] = ["completed", "complete", "abgeschlossen", "terminé", "termine", "completado", "completada", "הושלם", "בוצע"];

/// Outgoing amount as a positive number: "-9.99", "−9,99", "(9.99)". Incoming/zero → None.
fn outgoing(g: &str) -> Option<f64> {
    let t = g.trim();
    let neg = t.starts_with(['-', '−']) || t.ends_with('-') || (t.starts_with('(') && t.ends_with(')'));
    let v = parse_price(&t.replace(['-', '−', '(', ')'], ""))?;
    (neg && v > 0.0).then_some(v)
}

/// Numeric a/b/y parts of "3/11/2026", "03.11.26", "3-11-2026".
fn numeric(s: &str) -> Option<(u32, u32, i32)> {
    let s = s.trim();
    let s = s.split([' ', 'T']).next().unwrap_or(s);
    let p: Vec<&str> = s.split(['/', '.', '-']).collect();
    if p.len() != 3 || p[0].len() > 2 || p[1].len() > 2 || !p.iter().all(|x| !x.is_empty() && x.bytes().all(|b| b.is_ascii_digit())) {
        return None;
    }
    let y: i32 = p[2].parse().ok()?;
    Some((p[0].parse().ok()?, p[1].parse().ok()?, if y < 100 { 2000 + y } else { y }))
}

fn ymd(y: i32, m: u32, d: u32) -> Option<i32> {
    let z = data::days_from_civil(y, m, d);
    ((1..=12).contains(&m) && (1..=31).contains(&d) && data::civil_from_days(z) == (y, m, d)).then_some(z)
}

/// Out-of-order steps when `dates` (file order) are read with one day/month order: the fewer of
/// ascending/descending violations, unreadable dates counted as violations.
fn disorder(dates: &[Option<i32>]) -> usize {
    let bad = dates.iter().filter(|d| d.is_none()).count();
    let v: Vec<i32> = dates.iter().flatten().copied().collect();
    let up = v.windows(2).filter(|w| w[1] < w[0]).count();
    let down = v.windows(2).filter(|w| w[1] > w[0]).count();
    bad * 1000 + up.min(down)
}

/// Parses every row's date with one day/month order for the whole file: a part over 12 decides;
/// otherwise the order whose dates run monotonic; tie → day first. ISO and month-name dates
/// ("03-Nov-2026", "Nov 3, 2026") are read directly.
pub fn parse_dates(rows: &[PaypalRow]) -> Result<Vec<Option<i32>>, String> {
    let nums: Vec<Option<(u32, u32, i32)>> = rows.iter().map(|r| numeric(&r.date)).collect();
    let (dm_forced, md_forced) = (nums.iter().flatten().any(|n| n.0 > 12), nums.iter().flatten().any(|n| n.1 > 12));
    if dm_forced && md_forced {
        return Err(DATE_ERROR.into());
    }
    let read = |day_first: bool| -> Vec<Option<i32>> {
        rows.iter()
            .zip(&nums)
            .map(|(r, n)| match n {
                Some((a, b, y)) => if day_first { ymd(*y, *b, *a) } else { ymd(*y, *a, *b) },
                None => data::parse_iso(&r.date.replace(['/', '.'], "-")).or_else(|| first_date(&r.date, 0).filter(|_| has_year(&r.date))),
            })
            .collect()
    };
    let out = if dm_forced {
        read(true)
    } else if md_forced {
        read(false)
    } else {
        let (dm, md) = (read(true), read(false));
        if disorder(&md) < disorder(&dm) { md } else { dm }
    };
    if !rows.is_empty() && out.iter().all(Option::is_none) {
        return Err(DATE_ERROR.into());
    }
    Ok(out)
}

fn has_year(s: &str) -> bool {
    s.split(|c: char| !c.is_ascii_digit()).any(|p| p.len() == 4)
}

/// Completed outgoing payments → charges. Subscription / pre-approved / automatic types count
/// from one row; other merchants need ≥ 2 charges at a regular interval with amounts within ±10%.
pub fn charges_from_paypal(rows: &[PaypalRow]) -> Result<Vec<Charge>, String> {
    if rows.len() > MAX_ROWS {
        return Err("This file has more than 20,000 rows. Download a shorter period from PayPal.".into());
    }
    let dates = parse_dates(rows)?;
    let mut all: Vec<Charge> = Vec::new();
    for (r, d) in rows.iter().zip(dates) {
        let (typ, status) = (r.typ.to_lowercase(), r.status.to_lowercase());
        let (Some(date), Some(price)) = (d, outgoing(&r.gross)) else { continue };
        let cur = r.cur.trim().to_ascii_uppercase();
        let name = r.name.trim();
        if name.is_empty() || cur.len() != 3 || DROP.iter().any(|w| typ.contains(w))
            || !(status.trim().is_empty() || DONE.iter().any(|w| status.contains(w)))
        {
            continue;
        }
        let svc = services::by_merchant(name);
        let mut flags = RECEIPT | AMOUNT;
        if svc.is_some() {
            flags |= KNOWN;
        }
        if RECURRING.iter().any(|w| typ.contains(w)) {
            flags |= RECUR;
        }
        let (name, site, category) = svc.map_or((name.to_string(), String::new(), String::new()), |s| {
            (s.name.to_string(), s.site.to_string(), s.category.to_string())
        });
        let key = norm(&name);
        if key.is_empty() {
            continue;
        }
        let price = super::round2(price);
        all.push(Charge { key, name, date, price, cur, site, category, score_parts: flags, src: 1, method: "PayPal".into(), ..Default::default() });
    }
    // Keep one-off merchants only when they look recurring.
    let mut by: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, c) in all.iter().enumerate() {
        by.entry(c.key.clone()).or_default().push(i);
    }
    let mut keep = vec![false; all.len()];
    for idx in by.values() {
        if idx.iter().any(|&i| all[i].score_parts & RECUR != 0) {
            idx.iter().for_each(|&i| keep[i] = true);
            continue;
        }
        let latest = idx.iter().copied().max_by_key(|&i| all[i].date).unwrap_or(idx[0]);
        let (lp, lc) = (all[latest].price, &all[latest].cur);
        let mut near: Vec<usize> = idx.iter().copied().filter(|&i| &all[i].cur == lc && (all[i].price - lp).abs() <= lp * 0.1).collect();
        near.sort_by_key(|&i| all[i].date);
        let ds: Vec<i32> = near.iter().map(|&i| all[i].date).collect();
        if near.len() >= 2 && period_from_gaps(&ds).is_some() {
            near.iter().for_each(|&i| keep[i] = true);
        }
    }
    Ok(all.into_iter().zip(keep).filter(|x| x.1).map(|x| x.0).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::group;

    /// Test-only CSV reader (the app's reader is in ui.html): quoted fields, BOM, CRLF.
    fn csv(text: &str) -> Vec<Vec<String>> {
        let mut rows = Vec::new();
        for line in text.trim_start_matches('\u{feff}').lines().filter(|l| !l.trim().is_empty()) {
            let (mut row, mut cur, mut q) = (Vec::new(), String::new(), false);
            let mut it = line.chars().peekable();
            while let Some(c) = it.next() {
                match (c, q) {
                    ('"', true) if it.peek() == Some(&'"') => {
                        cur.push('"');
                        it.next();
                    }
                    ('"', _) => q = !q,
                    (',', false) => row.push(std::mem::take(&mut cur)),
                    _ => cur.push(c),
                }
            }
            row.push(cur);
            rows.push(row);
        }
        rows
    }

    /// Maps columns the way the JS mapper does: first alias present in the header.
    fn rows_of(text: &str) -> Vec<PaypalRow> {
        let t = csv(text);
        let a = paypal_column_aliases();
        let col = |f: &str| {
            a[f].as_array().unwrap().iter().find_map(|n| t[0].iter().position(|h| h.trim() == n.as_str().unwrap()))
        };
        let c: Vec<Option<usize>> = ["date", "name", "type", "status", "currency", "gross"].iter().map(|f| col(f)).collect();
        let get = |r: &Vec<String>, i: usize| c[i].and_then(|k| r.get(k)).cloned().unwrap_or_default();
        t[1..]
            .iter()
            .map(|r| PaypalRow { date: get(r, 0), name: get(r, 1), typ: get(r, 2), status: get(r, 3), cur: get(r, 4), gross: get(r, 5) })
            .collect()
    }

    fn day(s: &str) -> i32 {
        data::parse_iso(s).unwrap()
    }

    fn row(date: &str) -> PaypalRow {
        PaypalRow { date: date.into(), ..Default::default() }
    }

    #[test]
    fn paypal_date_order() {
        let p = |v: &[&str]| parse_dates(&v.iter().map(|d| row(d)).collect::<Vec<_>>());
        // A part over 12 decides for the whole file.
        assert_eq!(p(&["03/04/2026", "13/04/2026"]).unwrap(), vec![Some(day("2026-04-03")), Some(day("2026-04-13"))]);
        assert_eq!(p(&["04/03/2026", "04/13/2026"]).unwrap(), vec![Some(day("2026-04-03")), Some(day("2026-04-13"))]);
        // No part over 12: the order that runs monotonic wins (Jan 2, Feb 1, Mar 3 both times).
        assert_eq!(p(&["01/02/2026", "02/01/2026", "03/03/2026"]).unwrap()[0], Some(day("2026-01-02")));
        assert_eq!(p(&["02/01/2026", "01/02/2026", "03/03/2026"]).unwrap()[0], Some(day("2026-01-02")));
        // Both orders monotonic: day first.
        assert_eq!(p(&["12/05/2026", "11/05/2026"]).unwrap()[0], Some(day("2026-05-12")));
        // ISO and month names.
        assert_eq!(p(&["2026-11-03", "03-Nov-2026", "Nov 3, 2026"]).unwrap(), vec![Some(day("2026-11-03")); 3]);
        assert!(p(&["13/01/2026", "01/13/2026"]).is_err());
        assert!(p(&["yesterday"]).is_err());
        assert_eq!(outgoing("-1.234,56"), Some(1234.56));
        assert_eq!(outgoing("(9.99)"), Some(9.99));
        assert_eq!(outgoing("9.99"), None);
    }

    #[test]
    fn paypal_csv_en() {
        let rows = rows_of(include_str!("fixtures/paypal_en.csv"));
        let ch = charges_from_paypal(&rows).unwrap();
        let names: Vec<&str> = ch.iter().map(|c| c.name.as_str()).collect();
        // Netflix (subscription type, 3), Acme VPN (pre-approved, 1), Acme Hosting (regular, 3);
        // not the one-off shop, the conversion pair, the refund or the pending row.
        assert_eq!(ch.len(), 7, "{:?}", names);
        assert!(!names.contains(&"Some Shop") && !names.contains(&"Pending Co"));
        let g = group(&ch, day("2026-11-05")); // before the single Acme VPN charge goes stale
        let n = g.iter().find(|c| c.key == "netflix").unwrap();
        assert_eq!((n.name.as_str(), n.price, n.cur.as_str(), n.count, n.every, n.unit, n.src), ("Netflix", 15.49, "USD", 3, 1, 2, 1));
        assert_eq!(n.next, day("2026-12-03"));
        let v = g.iter().find(|c| c.key == "acmevpn").unwrap();
        assert_eq!((v.price, v.score), (4.99, 50));
        let h = g.iter().find(|c| c.key == "acmehosting").unwrap();
        assert_eq!((h.price, h.count, h.score), (5.5, 3, 60));
    }

    #[test]
    fn paypal_csv_he() {
        let rows = rows_of(include_str!("fixtures/paypal_he.csv"));
        let ch = charges_from_paypal(&rows).unwrap();
        assert_eq!(ch.len(), 3, "{:?}", ch);
        assert!(ch.iter().all(|c| c.name == "Spotify" && c.cur == "ILS" && c.price == 19.9));
        assert_eq!(ch.iter().map(|c| c.date).max(), Some(day("2026-11-24")));
        assert!(charges_from_paypal(&vec![PaypalRow::default(); MAX_ROWS + 1]).is_err());
    }
}
