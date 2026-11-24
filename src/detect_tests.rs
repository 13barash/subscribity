//! Detector tests (fixtures in src/fixtures are anonymized).
use super::*;
use crate::mime::{parse, split_eml, RawMail};

fn day(s: &str) -> i32 {
    data::parse_iso(s).unwrap()
}

fn mail(eml: &[u8]) -> Option<Charge> {
    let (header, body) = split_eml(eml);
    let p = parse(&RawMail { header, body });
    charge_from_mail(&p, day("2026-11-24"))
}

fn check(c: &Charge, name: &str, price: f64, cur: &str, period: Option<(u32, u8)>) {
    assert_eq!((c.name.as_str(), c.price, c.cur.as_str(), c.period), (name, price, cur, period), "{:?}", c);
}

#[test]
fn detect_parse_price_vectors() {
    let v = [
        ("9,99", Some(9.99)), ("1,234", Some(1234.0)), ("1.234,56", Some(1234.56)), ("1 234,50", Some(1234.5)),
        ("54.90", Some(54.9)), ("1,299.00", Some(1299.0)), ("1\u{a0}234,50", Some(1234.5)), ("₪54.90", Some(54.9)),
        ("9.99", Some(9.99)), ("0,500", Some(0.5)), ("-5", None), ("− 5", None), ("", None), ("abc", None), ("12.", Some(12.0)),
    ];
    for (t, want) in v {
        assert_eq!(parse_price(t), want, "{:?}", t);
    }
}

#[test]
fn detect_amount_scanner() {
    let a = |t: &str| amounts(t).iter().map(|a| (a.v, a.cur)).collect::<Vec<_>>();
    assert_eq!(a("Total $9.99 USD"), vec![(9.99, "USD")]);
    assert_eq!(a("you paid 39,90 ₪ and C$5 and R$ 1.234,56"), vec![(39.9, "ILS"), (5.0, "CAD"), (1234.56, "BRL")]);
    assert_eq!(a("99.90 ש\"ח, 1 234,50 EUR, 12 kr, 2026 $5"), vec![(99.9, "ILS"), (1234.5, "EUR"), (5.0, "USD")]);
    assert_eq!(a("Discount -$2.00 then US$7"), vec![(7.0, "USD")]);
    let t = "subtotal $10.00 tax $1.00 total $11.00";
    assert_eq!(pick(&amounts(t), t).map(|a| a.v), Some(11.0));
}

#[test]
fn detect_period_and_dates() {
    assert_eq!(period_in("only $9.99/mo"), Some((1, 2)));
    assert_eq!(period_in("billed annually"), Some((1, 3)));
    assert_eq!(period_in("semi-annual plan"), Some((6, 2)));
    assert_eq!(period_in("billed every 2 months"), Some((2, 2)));
    assert_eq!(period_in("מנוי שנתי"), Some((1, 3)));
    assert_eq!(period_in("3 חודשים"), None);
    assert_eq!(period_in("bimonthly newsletter"), None);
    let near = day("2026-11-24");
    assert_eq!(first_date("on December 5, 2026.", near), Some(day("2026-12-05")));
    assert_eq!(first_date("5 Dec 2026", near), Some(day("2026-12-05")));
    assert_eq!(first_date(": 2026-12-05", near), Some(day("2026-12-05")));
    assert_eq!(first_date("jan 3rd", near), Some(day("2027-01-03")));
    assert_eq!(first_date("24/12/2026", near), Some(day("2026-12-24")));
    assert_eq!(first_date("05/06/2026", near), None);
    assert_eq!(first_date("february 30, 2026", near), None);
}

#[test]
fn detect_fixture_netflix() {
    let c = mail(include_bytes!("fixtures/netflix_qp.eml")).unwrap();
    check(&c, "Netflix", 54.9, "ILS", Some((1, 2)));
    assert_eq!((c.renews, c.site.as_str(), c.key.as_str(), c.trial), (Some(day("2026-12-24")), "netflix.com", "netflix", false));
    assert_eq!(c.score_parts & (KNOWN | RECEIPT | AMOUNT | PERIOD), KNOWN | RECEIPT | AMOUNT | PERIOD);
}

#[test]
fn detect_fixture_spotify() {
    let c = mail(include_bytes!("fixtures/spotify_b64.eml")).unwrap();
    check(&c, "Spotify Premium", 11.99, "USD", Some((1, 2)));
}

#[test]
fn detect_fixture_paypal() {
    let c = mail(include_bytes!("fixtures/paypal_en.eml")).unwrap();
    check(&c, "Acme Cloud Ltd", 9.99, "USD", None);
    assert_eq!((c.key.as_str(), c.score_parts & (KNOWN | RECUR)), ("acmecloud", RECUR));
    let c = mail(include_bytes!("fixtures/paypal_he.eml")).unwrap();
    check(&c, "Netflix", 54.9, "ILS", Some((1, 2)));
    assert_ne!(c.score_parts & KNOWN, 0);
}

#[test]
fn detect_fixture_stores() {
    let c = mail(include_bytes!("fixtures/stripe.eml")).unwrap();
    check(&c, "Acme Notes", 8.0, "EUR", Some((1, 2)));
    let c = mail(include_bytes!("fixtures/googleplay.eml")).unwrap();
    check(&c, "YouTube Premium", 23.9, "ILS", Some((1, 2)));
    assert_ne!(c.score_parts & KNOWN, 0);
    let c = mail(include_bytes!("fixtures/apple.eml")).unwrap();
    check(&c, "iCloud+ 50 GB", 0.99, "USD", Some((1, 2)));
    assert_eq!((c.score_parts & KNOWN, c.renews), (KNOWN, Some(day("2026-12-02"))));
}

#[test]
fn detect_fixture_charsets_and_truncation() {
    let c = mail(include_bytes!("fixtures/win1252.eml")).unwrap();
    check(&c, "Acme Tools", 4.99, "EUR", Some((1, 2)));
    assert_eq!(c.site, "acme-tools.de");
    let c = mail(include_bytes!("fixtures/iso8859_8_subject.eml")).unwrap();
    check(&c, "HOT", 99.9, "ILS", Some((1, 2)));
    let c = mail(include_bytes!("fixtures/truncated_b64.eml")).unwrap();
    check(&c, "Dropbox Plus", 11.99, "USD", Some((1, 2)));
}

#[test]
fn detect_fixture_rejects_and_trial() {
    assert_eq!(mail(include_bytes!("fixtures/marketing.eml")), None);
    assert_eq!(mail(include_bytes!("fixtures/refund.eml")), None);
    let c = mail(include_bytes!("fixtures/trial.eml")).unwrap();
    check(&c, "Duolingo Super", 12.99, "USD", Some((1, 2)));
    assert_eq!((c.trial, c.renews), (true, Some(day("2026-12-01"))));
}

fn mails(eml: &[u8]) -> Vec<Charge> {
    let (header, body) = split_eml(eml);
    charges_from_mail(&parse(&RawMail { header, body }), day("2026-11-24"))
}

#[test]
fn detect_fixture_plan_upgrade() {
    // Plan from the receipt line, the plan's price (not the prorated total), card brand from
    // the HTML logo with the digits from the text part; grouped under the service key.
    let c = mail(include_bytes!("fixtures/claude_upgrade.eml")).unwrap();
    check(&c, "Claude Ultra", 579.0, "ILS", None);
    assert_eq!((c.key.as_str(), c.method.as_str(), c.category.as_str()), ("claudepro", "Mastercard 0000", "AI"));
}

#[test]
fn detect_fixture_apple_items() {
    // One charge per subscription item; the one-off in-app purchase is skipped.
    let cs = mails(include_bytes!("fixtures/apple_items.eml"));
    let got: Vec<_> = cs.iter().map(|c| (c.name.as_str(), c.price, c.period, c.category.as_str(), c.method.as_str())).collect();
    assert_eq!(got, vec![
        ("Apple Developer Program", 324.9, Some((1, 3)), "Dev", "Mastercard 0000"),
        ("iCloud+ 2 TB", 39.9, Some((1, 2)), "Cloud", "Mastercard 0000"),
    ]);
    assert_eq!((cs[0].renews, cs[1].renews), (Some(day("2027-11-14")), Some(day("2026-12-16"))));
}

#[test]
fn detect_rejects_non_charges() {
    let p = |from: &str, subj: &str, text: &str| {
        let p = Parsed { from_addr: from.into(), subject: subj.into(), text: text.into(), ..Default::default() };
        charges_from_mail(&p, day("2026-11-24"))
    };
    // Promos with order/purchase words, shipping updates, declined payments, payouts, credits.
    assert!(p("news@shop.example.com", "Special offer: $30 off your first order", "Order now, total $30").is_empty());
    assert!(p("news@shop.example.com", "Gift with $100+ Purchase", "Spend $100").is_empty());
    assert!(p("orders@shop.example.com", "Order Shipped #000000", "Order total ₪1,029.19").is_empty());
    assert!(p("orders@shop.example.com", "Payment declined: update your information", "Order total $30.00").is_empty());
    assert!(p("noreply@study.example.com", "Your payout receipt", "Payout of $55.00 sent").is_empty());
    assert!(p("billing@anthropic.com", "$200 USD in API credits was added", "Your API credits were added: $200 USD").is_empty());
    // A 0.00 order is not a trial; "$65k revenue" is not a price.
    assert!(p("support@registrar.example.com", "Order Summary (Order# 000)", "Final Cost : $0.00").is_empty());
    assert!(p("notify@market.example.com", "Your receipt: new listing", "$65k TTM revenue").is_empty());
    // Advice about auto-renewal in boilerplate is not subscription wording.
    let c = p("support@registrar.example.com", "Order Summary (Order# 000)", "Final Cost : $12.28\nWe encourage you to select auto-renewal for your domains.");
    assert_eq!(c[0].score_parts & RECUR, 0);
    // A cancellation notice is an ENDED marker under the service key.
    let c = p("no-reply@canva.com", "Your Canva Pro subscription has ended", "Your subscription has been cancelled.");
    assert_eq!((c[0].key.as_str(), c[0].score_parts & ENDED), ("canvapro", ENDED));
}

#[test]
fn detect_no_panic_on_any_cut() {
    let all: [&[u8]; 15] = [
        include_bytes!("fixtures/netflix_qp.eml"), include_bytes!("fixtures/spotify_b64.eml"),
        include_bytes!("fixtures/paypal_en.eml"), include_bytes!("fixtures/paypal_he.eml"),
        include_bytes!("fixtures/stripe.eml"), include_bytes!("fixtures/googleplay.eml"),
        include_bytes!("fixtures/apple.eml"), include_bytes!("fixtures/win1252.eml"),
        include_bytes!("fixtures/iso8859_8_subject.eml"), include_bytes!("fixtures/marketing.eml"),
        include_bytes!("fixtures/refund.eml"), include_bytes!("fixtures/trial.eml"),
        include_bytes!("fixtures/truncated_b64.eml"), include_bytes!("fixtures/claude_upgrade.eml"),
        include_bytes!("fixtures/apple_items.eml"),
    ];
    for eml in all {
        let (header, body) = split_eml(eml);
        for n in 0..=body.len() {
            let p = parse(&RawMail { header, body: &body[..n] });
            let _ = charge_from_mail(&p, 0);
        }
        for n in 0..=header.len() {
            let p = parse(&RawMail { header: &header[..n], body });
            let _ = charge_from_mail(&p, 0);
        }
    }
    for s in ["₪", "שילמת ל", "€1", "1 ", "every 99999999999999999999 months", "renews ש 31/", "item (", "<a b='", "&#xFFFFFFFF;", "=?", "payment to ."] {
        let p = Parsed { from_addr: "a@paypal.com".into(), subject: s.into(), text: s.into(), ..Default::default() };
        let _ = charge_from_mail(&p, 0);
        let _ = crate::mime::html_to_text(s);
    }
}

fn ch(key: &str, date: &str, price: f64, flags: u16) -> Charge {
    Charge {
        key: key.into(), name: key.into(), date: day(date), price, cur: "USD".into(), period: None, trial: false,
        renews: None, site: String::new(), category: String::new(), score_parts: flags, src: 0, method: String::new(),
    }
}

#[test]
fn detect_group_periods() {
    let f = RECEIPT | AMOUNT | RECUR;
    // 31st → 28 Feb, then back to the 31st.
    let g = group(&[ch("acme", "2026-12-31", 5.0, f), ch("acme", "2027-01-31", 5.0, f)], day("2027-02-01"));
    assert_eq!((g[0].every, g[0].unit, g[0].next, g[0].score, g[0].count), (1, 2, day("2027-02-28"), 50, 2));
    let g = group(&[ch("acme", "2027-01-31", 5.0, f), ch("acme", "2027-02-28", 5.0, f)], day("2027-03-01"));
    assert_eq!(g[0].next, day("2027-03-31"));
    // A skipped month stays monthly (median of 59 and 31 is 45).
    let g = group(&[ch("acme", "2026-01-05", 5.0, f), ch("acme", "2026-03-05", 5.0, f), ch("acme", "2026-04-05", 6.0, f)], day("2026-04-20"));
    assert_eq!((g[0].every, g[0].unit, g[0].price, g[0].next, g[0].score), (1, 2, 6.0, day("2026-05-05"), 60));
    // Yearly and weekly from gaps; irregular gaps → no period score → dropped below 40.
    assert_eq!(period_from_gaps(&[day("2025-03-01"), day("2026-03-02")]), Some((1, 3)));
    assert_eq!(period_from_gaps(&[0, 7, 14]), Some((1, 1)));
    assert_eq!(period_from_gaps(&[0, 45, 50]), None);
    let r = RECEIPT | AMOUNT;
    assert!(group(&[ch("x", "2026-01-01", 5.0, r), ch("x", "2026-02-15", 5.0, r)], day("2026-03-01")).is_empty());
    // Duplicate sightings of one charge merge; the known-service name wins; explicit renewal date wins.
    let mut a = ch("netflix", "2026-11-03", 15.49, f | RECUR);
    a.name = "Netflix.com".into();
    let mut b = ch("netflix", "2026-11-04", 15.49, KNOWN | f | PERIOD);
    b.name = "Netflix".into();
    b.period = Some((1, 2));
    b.renews = Some(day("2026-12-05"));
    let g = group(&[a, b], day("2026-11-24"));
    assert_eq!((g[0].name.as_str(), g[0].count, g[0].score, g[0].next), ("Netflix", 1, 90, day("2026-12-05")));
    // Service default period when nothing else is known.
    let g = group(&[ch("nordvpn", "2026-05-01", 99.0, KNOWN | f)], day("2026-11-24"));
    assert_eq!((g[0].every, g[0].unit, g[0].next), (1, 3, day("2027-05-01")));
}

#[test]
fn detect_group_evidence_first_and_end() {
    let r = RECEIPT | AMOUNT;
    // Shop orders without subscription wording: two or irregular/different amounts → nothing.
    let shop = [ch("shop", "2026-03-04", 85.2, r), ch("shop", "2026-04-05", 30.0, r), ch("shop", "2026-05-05", 59.15, r)];
    assert!(group(&shop, day("2026-06-01")).is_empty());
    // Three monthly charges of similar amounts are evidence on their own.
    let reg = [ch("gym", "2026-03-05", 50.0, r), ch("gym", "2026-04-05", 52.0, r), ch("gym", "2026-05-05", 50.0, r)];
    let g = group(&reg, day("2026-05-20"));
    assert_eq!((g[0].first, g[0].next, g[0].count), (day("2026-03-05"), day("2026-06-05"), 3));
    // Plan change (Pro → Max → Ultra within days): name and price from the last, first payment
    // where the current schedule starts, never in the future; latest payment method wins.
    let k = KNOWN | r;
    let mut a = ch("claudepro", "2026-11-05", 59.0, k);
    a.name = "Claude Pro".into();
    let mut b = ch("claudepro", "2026-11-08", 579.0, k);
    b.name = "Claude Ultra".into();
    b.method = "Mastercard 0000".into();
    let g = group(&[a, b], day("2026-11-24"));
    assert_eq!((g[0].name.as_str(), g[0].price, g[0].first, g[0].next), ("Claude Ultra", 579.0, day("2026-11-08"), day("2026-12-08")));
    let j = g[0].to_json();
    assert_eq!((j["first"].as_str(), j["start"].as_str(), j["payment"].as_str()), (Some("2026-11-08"), Some("2026-11-08"), Some("Mastercard 0000")));
    // A receipt's renewal date just passed (within the grace) rolls forward; it is the first payment.
    let mut n = ch("tracker", "2026-10-09", 99.9, k | RECUR);
    n.renews = Some(day("2026-11-09"));
    n.period = Some((1, 3));
    let g = group(&[n], day("2026-11-24"));
    assert_eq!((g[0].first, g[0].next), (day("2026-11-09"), day("2027-11-09")));
    // A cancellation notice after the last charge drops it; a charge after the notice keeps it.
    let mut end = ch("canva", "2026-09-10", 0.0, ENDED | KNOWN);
    end.cur = "XXX".into();
    let paid = ch("canva", "2026-08-09", 15.0, k);
    assert!(group(&[paid.clone(), end.clone()], day("2026-11-24")).is_empty());
    let again = ch("canva", "2026-11-09", 15.0, k);
    assert_eq!(group(&[paid, end, again], day("2026-11-24")).len(), 1);
    // Cents.
    let g = group(&[ch("acme", "2026-11-01", 59.14999999999999, k)], day("2026-11-24"));
    assert_eq!((g[0].price, g[0].to_json()["price"].as_f64()), (59.15, Some(59.15)));
}

#[test]
fn detect_mark_tracked() {
    let f = KNOWN | RECEIPT | AMOUNT;
    let mut g = group(&[ch("netflix", "2026-11-03", 17.99, f), ch("spotify", "2026-11-03", 11.99, f), ch("acme", "2026-11-03", 3.0, f)], day("2026-11-24"));
    g.iter_mut().find(|c| c.key == "acme").unwrap().site = "app.acme.io".into();
    let subs = vec![
        Sub { id: 7, name: "Netflix Premium".into(), price: 15.49, cur: "USD".into(), ..Default::default() },
        Sub { id: 8, name: "Music".into(), price: 11.99, cur: "USD".into(), url: "https://www.spotify.com/account".into(), ..Default::default() },
        Sub { id: 9, name: "Tools".into(), price: 3.0, cur: "USD".into(), url: "acme.io".into(), ..Default::default() },
    ];
    let skipped: HashSet<String> = ["spotify".to_string()].into();
    g.iter_mut().find(|c| c.key == "spotify").unwrap().site = "spotify.com".into();
    mark(&mut g, &subs, &skipped);
    let get = |k: &str| g.iter().find(|c| c.key == k).unwrap();
    assert_eq!((get("netflix").tracked, get("netflix").price_changed, get("netflix").sub_id), (true, true, 7));
    assert_eq!((get("spotify").tracked, get("spotify").price_changed, get("spotify").skipped), (true, false, true));
    assert_eq!((get("acme").tracked, get("acme").sub_id), (true, 9));
    let j = get("netflix").to_json();
    assert_eq!((j["next"].as_str(), j["priceChanged"].as_bool(), j["level"].as_str()), (Some("2026-12-03"), Some(true), Some("high")));
}

#[test]
fn detect_group_drops_stale() {
    let k = KNOWN | RECEIPT | AMOUNT | RECUR;
    let monthly = |d: &str| {
        let mut c = ch("acme", d, 9.99, k);
        c.period = Some((1, 2));
        c
    };
    // Monthly: last charge in June, nothing since → over; charged this month → kept.
    assert!(group(&[monthly("2026-05-07"), monthly("2026-06-07")], day("2026-11-24")).is_empty());
    let g = group(&[monthly("2026-09-13"), monthly("2026-10-13"), monthly("2026-11-13")], day("2026-11-24"));
    assert_eq!((g.len(), g[0].next), (1, day("2026-12-13")));
    // Within the 10-day grace a late receipt may still come.
    assert_eq!(group(&[monthly("2026-10-20")], day("2026-11-24")).len(), 1);
    assert!(group(&[monthly("2026-10-10")], day("2026-11-24")).is_empty());
    // Yearly renewal notice (no receipt) promising 9 Nov: passed with no receipt → over.
    let mut n = ch("tracker", "2026-10-09", 99.9, k | NOTICE | APPLE);
    n.renews = Some(day("2026-11-09"));
    n.period = Some((1, 3));
    assert!(group(&[n.clone()], day("2026-11-24")).is_empty());
    assert_eq!(group(&[n.clone()], day("2026-11-10")).len(), 1);
    // The renewal receipt came: kept, and the notice merged into a receipt is no longer a notice.
    let mut paid = ch("tracker", "2026-11-09", 99.9, k | APPLE);
    paid.period = Some((1, 3));
    let g = group(&[n.clone(), paid], day("2026-11-24"));
    assert_eq!((g.len(), g[0].next, g[0].apple), (1, day("2027-11-09"), true));
    // Trial whose end passed with no paid receipt → over.
    let mut t = ch("verified", "2026-06-07", 53.5, k | NOTICE);
    (t.trial, t.renews, t.period) = (true, Some(day("2026-07-07")), Some((1, 2)));
    assert!(group(&[t.clone()], day("2026-11-24")).is_empty());
    let g = group(&[t], day("2026-06-20"));
    assert_eq!((g.len(), g[0].trial), (1, true));
    // Yearly charged 11 months ago → kept; 13 months ago → over.
    let mut y = ch("nordvpn", "2025-12-20", 99.0, KNOWN | RECEIPT | AMOUNT);
    y.period = Some((1, 3));
    assert_eq!(group(&[y.clone()], day("2026-11-24"))[0].next, day("2026-12-20"));
    y.date = day("2025-10-01");
    assert!(group(&[y], day("2026-11-24")).is_empty());
}
