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
    pub manage: &'static str, // account / cancel page (https), "" = none known
    pub tpl: bool, // shown in the quick-add list
}

pub const APPLE_SUBS: &str = "https://apps.apple.com/account/subscriptions";
pub const GOOGLE_PLAY_SUBS: &str = "https://play.google.com/store/account/subscriptions";
pub const PAYPAL_AUTOPAY: &str = "https://www.paypal.com/myaccount/autopay/";

const fn s(name: &'static str, site: &'static str, category: &'static str, unit: u8, senders: &'static [&'static str], manage: &'static str, tpl: bool) -> Service {
    Service { name, site, category, every: 1, unit, senders, manage, tpl }
}
const M: u8 = 2;
const Y: u8 = 3;
const T: bool = true;
const F: bool = false;
const STORE: &str = APPLE_SUBS;
const NONE: &str = "";

/// The first 60 rows (tpl) are the quick-add templates: 10 categories x 6, in display order.
/// Senders are left empty where a shared store sends the receipt (Apple, Google, Amazon).
/// `manage` URLs are verified except those marked `unverified` (bot-blocked); empty = open the website.
pub static SERVICES: &[Service] = &[
    // Streaming
    s("Netflix", "netflix.com", "Streaming", M, &["netflix.com"], "https://www.netflix.com/account", T),
    s("Disney+", "disneyplus.com", "Streaming", M, &["disneyplus.com"], "https://www.disneyplus.com/account/subscription", T),
    s("YouTube Premium", "youtube.com", "Streaming", M, &["youtube.com"], "https://www.youtube.com/paid_memberships", T),
    s("Amazon Prime", "amazon.com", "Streaming", M, &[], "https://www.amazon.com/gp/primecentral", T),
    s("HBO Max", "hbomax.com", "Streaming", M, &["hbomax.com", "max.com"], "https://www.hbomax.com/account", T),
    s("Apple TV", "tv.apple.com", "Streaming", M, &[], STORE, T),
    // Music
    s("Spotify", "spotify.com", "Music", M, &["spotify.com"], "https://www.spotify.com/account/subscription/", T),
    s("Apple Music", "music.apple.com", "Music", M, &[], STORE, T),
    s("YouTube Music", "music.youtube.com", "Music", M, &[], "https://www.youtube.com/paid_memberships", T),
    s("Amazon Music Unlimited", "music.amazon.com", "Music", M, &[], "https://www.amazon.com/music/settings", T),
    s("Tidal", "tidal.com", "Music", M, &["tidal.com"], "https://account.tidal.com/subscription", T), // unverified (bot-blocked)
    s("Deezer", "deezer.com", "Music", M, &["deezer.com"], "https://www.deezer.com/account/subscription", T),
    // AI
    s("ChatGPT Plus", "chatgpt.com", "AI", M, &["openai.com", "chatgpt.com"], "https://chatgpt.com/#settings/Subscription", T),
    s("Claude Pro", "claude.ai", "AI", M, &["anthropic.com", "claude.ai"], "https://claude.ai/settings/billing", T),
    s("Google AI Pro", "gemini.google.com", "AI", M, &[], "https://myaccount.google.com/payments-and-subscriptions", T),
    s("Perplexity Pro", "perplexity.ai", "AI", M, &["perplexity.ai"], "https://www.perplexity.ai/settings/account", T), // unverified (bot-blocked)
    s("Midjourney", "midjourney.com", "AI", M, &["midjourney.com"], "https://www.midjourney.com/account", T),
    s("SuperGrok", "grok.com", "AI", M, &["grok.com", "x.ai"], NONE, T),
    // Cloud & storage
    s("iCloud+", "icloud.com", "Cloud & storage", M, &[], "https://www.icloud.com/settings/", T),
    s("Google One", "one.google.com", "Cloud & storage", M, &[], "https://one.google.com/settings", T),
    s("Dropbox", "dropbox.com", "Cloud & storage", M, &["dropbox.com", "dropboxmail.com"], "https://www.dropbox.com/account/plan", T),
    s("pCloud", "pcloud.com", "Cloud & storage", Y, &["pcloud.com"], NONE, T),
    s("MEGA", "mega.io", "Cloud & storage", M, &["mega.io", "mega.nz"], "https://mega.nz/fm/account/plan", T),
    s("Backblaze", "backblaze.com", "Cloud & storage", M, &["backblaze.com"], "https://secure.backblaze.com/billing.htm", T),
    // Productivity
    s("Microsoft 365", "microsoft.com", "Productivity", M, &["microsoft.com"], "https://account.microsoft.com/services", T),
    s("Notion", "notion.so", "Productivity", M, &["notion.so", "makenotion.com"], NONE, T),
    s("Adobe Creative Cloud", "adobe.com", "Productivity", M, &["adobe.com"], "https://account.adobe.com/plans", T),
    s("Canva Pro", "canva.com", "Productivity", M, &["canva.com"], "https://www.canva.com/settings/billing-and-teams", T),
    s("Grammarly", "grammarly.com", "Productivity", M, &["grammarly.com"], "https://account.grammarly.com/subscription", T),
    s("Todoist", "todoist.com", "Productivity", M, &["todoist.com", "doist.com"], "https://app.todoist.com/app/settings/subscription", T),
    // Gaming
    s("Xbox Game Pass", "xbox.com", "Gaming", M, &["xbox.com"], "https://account.microsoft.com/services", T),
    s("PlayStation Plus", "playstation.com", "Gaming", M, &["playstation.com"], "https://www.playstation.com/acct/management", T),
    s("Nintendo Switch Online", "nintendo.com", "Gaming", Y, &["nintendo.com"], "https://ec.nintendo.com/my/membership", T),
    s("EA Play", "ea.com", "Gaming", M, &["ea.com"], "https://myaccount.ea.com/cp-ui/subscription/index", T),
    s("GeForce NOW", "nvidia.com", "Gaming", M, &["nvidia.com"], "https://www.nvidia.com/en-us/account/gfn/", T),
    s("Discord Nitro", "discord.com", "Gaming", M, &["discord.com"], NONE, T),
    // News & reading
    s("The New York Times", "nytimes.com", "News & reading", M, &["nytimes.com"], "https://myaccount.nytimes.com/seg/subscription", T),
    s("The Washington Post", "washingtonpost.com", "News & reading", M, &["washingtonpost.com"], "https://www.washingtonpost.com/my-account/", T), // unverified (bot-blocked)
    s("The Economist", "economist.com", "News & reading", M, &["economist.com"], "https://myaccount.economist.com/", T),
    s("Medium", "medium.com", "News & reading", M, &["medium.com"], "https://medium.com/me/membership", T),
    s("Kindle Unlimited", "amazon.com", "News & reading", M, &[], "https://www.amazon.com/kindle-dbs/ku/ku-central", T),
    s("Audible", "audible.com", "News & reading", M, &["audible.com", "audible.co.uk", "audible.de"], "https://www.audible.com/account/overview", T),
    // Fitness & health
    s("Strava", "strava.com", "Fitness & health", M, &["strava.com"], "https://www.strava.com/account", T),
    s("Peloton", "onepeloton.com", "Fitness & health", M, &["onepeloton.com"], "", T),
    s("Headspace", "headspace.com", "Fitness & health", M, &["headspace.com"], "https://www.headspace.com/subscription/manage", T),
    s("Calm", "calm.com", "Fitness & health", Y, &["calm.com"], "", T),
    s("MyFitnessPal", "myfitnesspal.com", "Fitness & health", Y, &["myfitnesspal.com"], "", T),
    s("Apple Fitness+", "fitness.apple.com", "Fitness & health", M, &[], STORE, T),
    // VPN & security
    s("NordVPN", "nordvpn.com", "VPN & security", Y, &["nordvpn.com"], "https://my.nordaccount.com/billing/my-subscriptions/", T),
    s("ExpressVPN", "expressvpn.com", "VPN & security", Y, &["expressvpn.com"], "https://www.expressvpn.com/subscriptions", T),
    s("Surfshark", "surfshark.com", "VPN & security", Y, &["surfshark.com"], "https://my.surfshark.com/account/subscription", T),
    s("Proton", "proton.me", "VPN & security", M, &["proton.me", "protonmail.com"], "https://account.proton.me/dashboard", T),
    s("1Password", "1password.com", "VPN & security", Y, &["1password.com"], "https://my.1password.com/billing", T),
    s("Bitwarden", "bitwarden.com", "VPN & security", Y, &["bitwarden.com"], "https://vault.bitwarden.com/#/settings/subscription", T),
    // Developer tools
    s("GitHub Copilot", "github.com", "Developer tools", M, &["github.com"], "https://github.com/settings/copilot", T),
    s("JetBrains", "jetbrains.com", "Developer tools", Y, &["jetbrains.com"], "https://account.jetbrains.com/licenses", T),
    s("Cursor", "cursor.com", "Developer tools", M, &["cursor.com", "cursor.sh"], "https://cursor.com/dashboard", T), // unverified (bot-blocked)
    s("Figma", "figma.com", "Developer tools", M, &["figma.com"], NONE, T),
    s("Vercel", "vercel.com", "Developer tools", M, &["vercel.com"], "https://vercel.com/account/billing", T),
    s("DigitalOcean", "digitalocean.com", "Developer tools", M, &["digitalocean.com"], "https://cloud.digitalocean.com/account/billing", T),
    // Scanner-only rows (never shown in quick add).
    s("Prime Video", "primevideo.com", "Streaming", M, &["primevideo.com"], "https://www.amazon.com/gp/primecentral", F),
    s("Max", "max.com", "Streaming", M, &[], NONE, F),
    s("Paramount+", "paramountplus.com", "Streaming", M, &["paramountplus.com"], NONE, F),
    s("Hulu", "hulu.com", "Streaming", M, &["hulu.com"], NONE, F),
    s("Crunchyroll", "crunchyroll.com", "Streaming", M, &["crunchyroll.com"], NONE, F),
    s("Peacock", "peacocktv.com", "Streaming", M, &["peacocktv.com"], NONE, F),
    s("DAZN", "dazn.com", "Streaming", M, &["dazn.com"], NONE, F),
    s("SoundCloud", "soundcloud.com", "Music", M, &["soundcloud.com"], NONE, F),
    s("Evernote", "evernote.com", "Productivity", M, &["evernote.com"], NONE, F),
    s("Zoom", "zoom.us", "Work", M, &["zoom.us"], NONE, F),
    s("Slack", "slack.com", "Work", M, &["slack.com"], NONE, F),
    s("LinkedIn Premium", "linkedin.com", "Work", M, &["linkedin.com"], NONE, F),
    s("Duolingo Super", "duolingo.com", "Education", M, &["duolingo.com"], NONE, F),
    s("Patreon", "patreon.com", "Creators", M, &["patreon.com"], NONE, F),
    s("Setapp", "setapp.com", "Productivity", M, &["setapp.com"], NONE, F),
    s("Squarespace", "squarespace.com", "Work", M, &["squarespace.com"], NONE, F),
    s("Wix", "wix.com", "Work", Y, &["wix.com"], NONE, F),
    s("Haaretz", "haaretz.co.il", "News", M, &["haaretz.co.il", "haaretz.com"], NONE, F),
    s("EA", "ea.com", "Gaming", M, &[], NONE, F),
    s("MasterClass", "masterclass.com", "Education", Y, &["masterclass.com"], NONE, F),
    s("Coursera Plus", "coursera.org", "Education", M, &["coursera.org"], NONE, F),
    // App Store subscriptions seen on Apple receipts (no own sender: Apple sends the receipt).
    s("Instagram Plus", "instagram.com", "Social", M, &[], STORE, F),
    s("Meta Verified", "meta.com", "Social", M, &[], STORE, F),
    s("Telegram Premium", "telegram.org", "Social", M, &[], STORE, F),
    s("MacroFactor", "macrofactorapp.com", "Fitness", Y, &[], STORE, F),
    s("Chess.com", "chess.com", "Gaming", Y, &[], STORE, F),
    // Telecom (Israel).
    s("Partner", "partner.co.il", "Telecom", M, &["partner.co.il"], NONE, F),
    s("Cellcom", "cellcom.co.il", "Telecom", M, &["cellcom.co.il"], NONE, F),
    s("Pelephone", "pelephone.co.il", "Telecom", M, &["pelephone.co.il"], NONE, F),
    s("HOT", "hot.net.il", "Telecom", M, &["hot.net.il"], NONE, F),
    s("yes", "yes.co.il", "Telecom", M, &["yes.co.il"], NONE, F),
    s("Bezeq", "bezeq.co.il", "Telecom", M, &["bezeq.co.il"], NONE, F),
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

/// Hosts shared by several unrelated services: a website there says nothing about the service.
const SHARED_HOSTS: [&str; 2] = ["amazon.com", "google.com"];

/// Built-in manage / cancel page: the service `by_merchant(name)` finds, else the first service
/// with a page on the same registrable domain as `site` (so `www.netflix.com` works). "" = none.
pub fn manage_url(name: &str, site: &str) -> &'static str {
    if let Some(s) = by_merchant(name) {
        return s.manage;
    }
    let host = site.trim().trim_start_matches("https://").trim_start_matches("http://");
    let host = host.split(['/', '?', '#']).next().unwrap_or("");
    let host = host.rsplit('@').next().unwrap_or("").split(':').next().unwrap_or("");
    let r = registrable(host);
    if !r.contains('.') || SHARED_HOSTS.contains(&r.as_str()) {
        return "";
    }
    SERVICES.iter().find(|s| !s.manage.is_empty() && registrable(s.site) == r).map_or("", |s| s.manage)
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

/// Quick-add templates: `[name, site, category]`, plus a 4th element `3` for yearly services.
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
        assert_eq!(by_sender("mail.max.com").map(|s| s.name), Some("HBO Max"));
        assert_eq!(by_sender("primevideo.com").map(|s| s.name), Some("Prime Video"));
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
        assert_eq!(m("Max"), Some("HBO Max"));
        assert_eq!(m("HBO Max"), Some("HBO Max"));
        assert_eq!(m("Apple TV+"), Some("Apple TV"));
        assert_eq!(m("Prime Video"), Some("Prime Video"));
        assert_eq!(m("Amazon Prime"), Some("Amazon Prime"));
        assert_eq!(m("Maxwell Shop"), None);
        assert_eq!(m("Acme Cloud Ltd"), None);
        assert_eq!(norm("Acme Cloud Ltd."), "acmecloud");
        assert_eq!(norm("Disney+"), "disney");
        assert_eq!(norm("Apple TV+"), norm("Apple TV"));
    }

    #[test]
    fn services_templates_shape() {
        let t = templates_json();
        let a = t.as_array().unwrap();
        assert_eq!(a.len(), 60);
        assert_eq!(a[0], json!(["Netflix", "netflix.com", "Streaming"]));
        assert_eq!(a[5], json!(["Apple TV", "tv.apple.com", "Streaming"]));
        assert_eq!(a[32], json!(["Nintendo Switch Online", "nintendo.com", "Gaming", 3]));
        assert_eq!(a[59], json!(["DigitalOcean", "digitalocean.com", "Developer tools"]));
        let mut cats: Vec<&str> = Vec::new();
        for r in a {
            let c = r[2].as_str().unwrap();
            if !cats.contains(&c) {
                cats.push(c);
            }
        }
        assert_eq!(cats.len(), 10);
        for c in &cats {
            assert_eq!(a.iter().filter(|r| r[2] == *c).count(), 6, "{}", c);
        }
        // Grouped: each category is one contiguous block of 6.
        for (i, r) in a.iter().enumerate() {
            assert_eq!(r[2].as_str().unwrap(), cats[i / 6]);
        }
        assert!(SERVICES.iter().filter(|s| s.tpl).count() == 60 && SERVICES.iter().take(60).all(|s| s.tpl));
        assert!(SERVICES.len() >= 70);
    }

    #[test]
    fn services_manage_links() {
        for s in SERVICES {
            assert!(s.manage.is_empty() || s.manage.starts_with("https://"), "{}", s.name);
        }
        assert!(APPLE_SUBS.starts_with("https://") && GOOGLE_PLAY_SUBS.starts_with("https://") && PAYPAL_AUTOPAY.starts_with("https://"));
        assert_eq!(manage_url("Netflix", ""), "https://www.netflix.com/account");
        assert_eq!(manage_url("Apple TV+", ""), APPLE_SUBS);
        assert_eq!(manage_url("My own thing", "https://www.netflix.com/login"), "https://www.netflix.com/account");
        assert_eq!(manage_url("Foo Bar", "billing.spotify.com"), "https://www.spotify.com/account/subscription/");
        assert_eq!(manage_url("Foo Bar", "amazon.com"), "");
        assert_eq!(manage_url("Foo Bar", "gemini.google.com"), "");
        assert_eq!(manage_url("Foo Bar", "example.org"), "");
        assert_eq!(manage_url("Foo Bar", ""), "");
        assert_eq!(manage_url("SuperGrok", "grok.com"), "");
    }
}
