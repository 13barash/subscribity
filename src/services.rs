//! Known subscription services (quick-add templates + scanner sender table) and payment
//! aggregators whose receipts name the real merchant.
use serde_json::{json, Value};

pub struct Service {
    pub name: &'static str,
    pub site: &'static str,
    pub category: &'static str,
    pub every: u32,
    pub unit: u8, // same codes as data::Sub: 2 month, 3 year
    pub senders: &'static [&'static str], // sender domains (subdomains match too)
    pub tpl: bool, // shown in the quick-add list (the old ui.html TEMPLATES rows)
}

const fn s(name: &'static str, site: &'static str, category: &'static str, unit: u8, senders: &'static [&'static str], tpl: bool) -> Service {
    Service { name, site, category, every: 1, unit, senders, tpl }
}
const M: u8 = 2;
const Y: u8 = 3;
const T: bool = true;
const F: bool = false;

/// The first rows (tpl) are the quick-add templates, in their original order. Senders are left
/// empty where a shared store sends the receipt (Apple, Google, Amazon).
pub static SERVICES: &[Service] = &[
    s("Netflix", "netflix.com", "Streaming", M, &["netflix.com"], T),
    s("Disney+", "disneyplus.com", "Streaming", M, &["disneyplus.com"], T),
    s("YouTube Premium", "youtube.com", "Streaming", M, &["youtube.com"], T),
    s("Prime Video", "primevideo.com", "Streaming", M, &["primevideo.com"], T),
    s("Max", "max.com", "Streaming", M, &["max.com", "hbomax.com"], T),
    s("Apple TV+", "tv.apple.com", "Streaming", M, &[], T),
    s("Paramount+", "paramountplus.com", "Streaming", M, &["paramountplus.com"], T),
    s("Spotify", "spotify.com", "Music", M, &["spotify.com"], T),
    s("Apple Music", "music.apple.com", "Music", M, &[], T),
    s("YouTube Music", "music.youtube.com", "Music", M, &[], T),
    s("Deezer", "deezer.com", "Music", M, &["deezer.com"], T),
    s("Audible", "audible.com", "Books", M, &["audible.com", "audible.co.uk", "audible.de"], T),
    s("Kindle Unlimited", "amazon.com", "Books", M, &[], T),
    s("iCloud+", "icloud.com", "Cloud", M, &[], T),
    s("Google One", "one.google.com", "Cloud", M, &[], T),
    s("Dropbox", "dropbox.com", "Cloud", M, &["dropbox.com", "dropboxmail.com"], T),
    s("Microsoft 365", "microsoft.com", "Productivity", M, &["microsoft.com"], T),
    s("Notion", "notion.so", "Productivity", M, &["notion.so", "makenotion.com"], T),
    s("ChatGPT Plus", "chatgpt.com", "AI", M, &["openai.com", "chatgpt.com"], T),
    s("Claude Pro", "claude.ai", "AI", M, &["anthropic.com", "claude.ai"], T),
    s("GitHub Copilot", "github.com", "Dev", M, &["github.com"], T),
    s("Adobe Creative Cloud", "adobe.com", "Creative", M, &["adobe.com"], T),
    s("Canva Pro", "canva.com", "Creative", M, &["canva.com"], T),
    s("Figma", "figma.com", "Creative", M, &["figma.com"], T),
    s("Xbox Game Pass", "xbox.com", "Gaming", M, &["xbox.com"], T),
    s("PlayStation Plus", "playstation.com", "Gaming", M, &["playstation.com"], T),
    s("Nintendo Switch Online", "nintendo.com", "Gaming", Y, &["nintendo.com"], T),
    s("Discord Nitro", "discord.com", "Social", M, &["discord.com"], T),
    s("Duolingo Super", "duolingo.com", "Education", M, &["duolingo.com"], T),
    s("LinkedIn Premium", "linkedin.com", "Work", M, &["linkedin.com"], T),
    s("NordVPN", "nordvpn.com", "Security", Y, &["nordvpn.com"], T),
    s("1Password", "1password.com", "Security", Y, &["1password.com"], T),
    s("Patreon", "patreon.com", "Creators", M, &["patreon.com"], T),
    // Scanner-only rows.
    s("Hulu", "hulu.com", "Streaming", M, &["hulu.com"], F),
    s("Crunchyroll", "crunchyroll.com", "Streaming", M, &["crunchyroll.com"], F),
    s("Peacock", "peacocktv.com", "Streaming", M, &["peacocktv.com"], F),
    s("DAZN", "dazn.com", "Streaming", M, &["dazn.com"], F),
    s("Tidal", "tidal.com", "Music", M, &["tidal.com"], F),
    s("SoundCloud", "soundcloud.com", "Music", M, &["soundcloud.com"], F),
    s("Backblaze", "backblaze.com", "Cloud", M, &["backblaze.com"], F),
    s("pCloud", "pcloud.com", "Cloud", Y, &["pcloud.com"], F),
    s("Proton", "proton.me", "Security", M, &["proton.me", "protonmail.com"], F),
    s("Evernote", "evernote.com", "Productivity", M, &["evernote.com"], F),
    s("Todoist", "todoist.com", "Productivity", M, &["todoist.com", "doist.com"], F),
    s("Grammarly", "grammarly.com", "Productivity", M, &["grammarly.com"], F),
    s("Zoom", "zoom.us", "Work", M, &["zoom.us"], F),
    s("Slack", "slack.com", "Work", M, &["slack.com"], F),
    s("JetBrains", "jetbrains.com", "Dev", Y, &["jetbrains.com"], F),
    s("Cursor", "cursor.com", "AI", M, &["cursor.com", "cursor.sh"], F),
    s("Midjourney", "midjourney.com", "AI", M, &["midjourney.com"], F),
    s("Perplexity Pro", "perplexity.ai", "AI", M, &["perplexity.ai"], F),
    s("Setapp", "setapp.com", "Productivity", M, &["setapp.com"], F),
    s("Squarespace", "squarespace.com", "Work", M, &["squarespace.com"], F),
    s("Wix", "wix.com", "Work", Y, &["wix.com"], F),
    s("DigitalOcean", "digitalocean.com", "Dev", M, &["digitalocean.com"], F),
    s("The New York Times", "nytimes.com", "News", M, &["nytimes.com"], F),
    s("The Washington Post", "washingtonpost.com", "News", M, &["washingtonpost.com"], F),
    s("The Economist", "economist.com", "News", M, &["economist.com"], F),
    s("Medium", "medium.com", "News", M, &["medium.com"], F),
    s("Haaretz", "haaretz.co.il", "News", M, &["haaretz.co.il", "haaretz.com"], F),
    s("EA Play", "ea.com", "Gaming", M, &["ea.com"], F),
    s("ExpressVPN", "expressvpn.com", "Security", Y, &["expressvpn.com"], F),
    s("Surfshark", "surfshark.com", "Security", Y, &["surfshark.com"], F),
    s("Bitwarden", "bitwarden.com", "Security", Y, &["bitwarden.com"], F),
    s("Strava", "strava.com", "Fitness", M, &["strava.com"], F),
    s("Peloton", "onepeloton.com", "Fitness", M, &["onepeloton.com"], F),
    s("Headspace", "headspace.com", "Fitness", M, &["headspace.com"], F),
    s("Calm", "calm.com", "Fitness", Y, &["calm.com"], F),
    s("MasterClass", "masterclass.com", "Education", Y, &["masterclass.com"], F),
    // App Store subscriptions seen on Apple receipts (no own sender: Apple sends the receipt).
    s("Instagram Plus", "instagram.com", "Social", M, &[], F),
    s("Meta Verified", "meta.com", "Social", M, &[], F),
    s("Telegram Premium", "telegram.org", "Social", M, &[], F),
    s("MyFitnessPal", "myfitnesspal.com", "Fitness", Y, &[], F),
    s("MacroFactor", "macrofactorapp.com", "Fitness", Y, &[], F),
    s("Chess.com", "chess.com", "Gaming", Y, &[], F),
    s("Coursera Plus", "coursera.org", "Education", M, &["coursera.org"], F),
    s("Partner", "partner.co.il", "Telecom", M, &["partner.co.il"], F),
    s("Cellcom", "cellcom.co.il", "Telecom", M, &["cellcom.co.il"], F),
    s("Pelephone", "pelephone.co.il", "Telecom", M, &["pelephone.co.il"], F),
    s("HOT", "hot.net.il", "Telecom", M, &["hot.net.il"], F),
    s("yes", "yes.co.il", "Telecom", M, &["yes.co.il"], F),
    s("Bezeq", "bezeq.co.il", "Telecom", M, &["bezeq.co.il"], F),
];

/// Second-level labels under which the registrable domain has three parts.
const SECOND_LEVEL: [&str; 14] = [
    "co.uk", "org.uk", "co.il", "org.il", "ac.il", "net.il", "gov.il", "com.au", "co.jp", "com.br",
    "co.nz", "co.za", "com.mx", "com.tr",
];

/// `mail.billing.acme.co.uk` → `acme.co.uk`.
pub fn registrable(domain: &str) -> String {
    let d = domain.trim().trim_end_matches('.').to_ascii_lowercase();
    let parts: Vec<&str> = d.split('.').collect();
    let n = parts.len();
    if n <= 2 {
        return d;
    }
    let k = if SECOND_LEVEL.contains(&parts[n - 2..].join(".").as_str()) { 3 } else { 2 };
    parts[n - k..].join(".")
}

fn on_domain(d: &str, base: &str) -> bool {
    d == base || (d.len() > base.len() && d.ends_with(base) && d.as_bytes()[d.len() - base.len() - 1] == b'.')
}

/// Service whose sender domain is `domain` or a parent of it.
pub fn by_sender(domain: &str) -> Option<&'static Service> {
    let d = domain.trim().trim_end_matches('.').to_ascii_lowercase();
    SERVICES.iter().find(|s| s.senders.iter().any(|x| on_domain(&d, x)))
}

/// Lowercase letters and digits of the words in `s`, with trailing legal/TLD words dropped:
/// "Netflix.com" → "netflix", "Acme Cloud Ltd." → "acmecloud", "Disney+" → "disney".
pub fn norm(s: &str) -> String {
    const SUFFIX: [&str; 26] = [
        "com", "net", "org", "io", "ai", "co", "il", "uk", "de", "inc", "llc", "ltd", "limited",
        "gmbh", "corp", "bv", "ab", "sa", "sarl", "plc", "pte", "pty", "ag", "srl", "sro", "us",
    ];
    let lower = s.to_lowercase();
    let mut w: Vec<&str> = lower.split(|c: char| !c.is_alphanumeric()).filter(|x| !x.is_empty()).collect();
    while w.len() > 1 && w.last().is_some_and(|x| SUFFIX.contains(x)) {
        w.pop();
    }
    w.concat()
}

/// Known service for a merchant name taken from a receipt or a PayPal row: equal names first,
/// then "Spotify AB" / "Google One 100 GB" style prefixes, or a sender domain label ("OpenAI").
pub fn by_merchant(name: &str) -> Option<&'static Service> {
    let n = norm(name);
    if n.len() < 3 {
        return None;
    }
    let label = |x: &&str| x.split('.').next().unwrap_or("").replace('-', "");
    SERVICES.iter().find(|s| norm(s.name) == n || s.senders.iter().any(|x| label(x) == n)).or_else(|| {
        SERVICES.iter().find(|s| {
            let sn = norm(s.name);
            (sn.len() >= 4 && n.starts_with(&sn)) || s.senders.iter().any(|x| {
                let l = label(x);
                l.len() >= 5 && n.starts_with(&l)
            })
        })
    })
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Aggregator {
    PayPal,
    Stripe,
    Paddle,
    FastSpring,
    GooglePlay,
    Apple,
}

/// Payment platforms: their receipts are not a service, the merchant is in the text.
pub fn aggregator(domain: &str) -> Option<Aggregator> {
    let r = registrable(domain);
    Some(match r.split('.').next().unwrap_or("") {
        "paypal" => Aggregator::PayPal,
        "stripe" => Aggregator::Stripe,
        "paddle" => Aggregator::Paddle,
        "fastspring" => Aggregator::FastSpring,
        "google" => Aggregator::GooglePlay,
        "apple" | "itunes" => Aggregator::Apple,
        _ => return None,
    })
}

/// Quick-add templates in the shape ui.html's TEMPLATES had: `[name, site, category]`, plus a
/// 4th element `3` for yearly services.
pub fn templates_json() -> Value {
    Value::Array(
        SERVICES
            .iter()
            .filter(|s| s.tpl)
            .map(|s| if s.unit == Y { json!([s.name, s.site, s.category, 3]) } else { json!([s.name, s.site, s.category]) })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn services_by_sender_and_domains() {
        assert_eq!(by_sender("info.account.Netflix.com").map(|s| s.name), Some("Netflix"));
        assert_eq!(by_sender("dropboxmail.com").map(|s| s.name), Some("Dropbox"));
        assert_eq!(by_sender("notnetflix.com").map(|s| s.name), None);
        assert_eq!(by_sender("billing.hot.net.il").map(|s| s.name), Some("HOT"));
        assert_eq!(registrable("mail.acme.co.uk"), "acme.co.uk");
        assert_eq!(registrable("a.b.example.com"), "example.com");
        assert_eq!(registrable("hot.net.il"), "hot.net.il");
        assert_eq!(aggregator("intl.paypal.com"), Some(Aggregator::PayPal));
        assert_eq!(aggregator("email.apple.com"), Some(Aggregator::Apple));
        assert_eq!(aggregator("netflix.com"), None);
        for s in SERVICES {
            for d in s.senders {
                assert!(aggregator(d).is_none(), "{} is an aggregator", d);
            }
        }
    }

    #[test]
    fn services_by_merchant() {
        let m = |n: &str| by_merchant(n).map(|s| s.name);
        assert_eq!(m("Netflix.com"), Some("Netflix"));
        assert_eq!(m("Spotify AB"), Some("Spotify"));
        assert_eq!(m("OpenAI, LLC"), Some("ChatGPT Plus"));
        assert_eq!(m("Google One 100 GB"), Some("Google One"));
        assert_eq!(m("YouTube"), Some("YouTube Premium"));
        assert_eq!(m("Max"), Some("Max"));
        assert_eq!(m("Maxwell Shop"), None);
        assert_eq!(m("Acme Cloud Ltd"), None);
        assert_eq!(norm("Acme Cloud Ltd."), "acmecloud");
        assert_eq!(norm("Disney+"), "disney");
    }

    #[test]
    fn services_templates_shape() {
        let t = templates_json();
        let a = t.as_array().unwrap();
        assert_eq!(a.len(), 33);
        assert_eq!(a[0], json!(["Netflix", "netflix.com", "Streaming"]));
        assert_eq!(a[26], json!(["Nintendo Switch Online", "nintendo.com", "Gaming", 3]));
        assert_eq!(a[32], json!(["Patreon", "patreon.com", "Creators"]));
        assert!(SERVICES.len() >= 70);
    }
}
