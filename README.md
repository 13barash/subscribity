# Subscribity

A tiny subscription tracker that lives in the Windows tray.

![Subscribity](screenshot.png)

Subscribity keeps a list of your subscriptions, shows what they cost per month and per year,
and reminds you before each one renews. It sits in the notification area using about
**2 MB of RAM**, and its window only exists while it is open.

## Features

- Subscriptions with price, currency, billing cycle (every N days, weeks, months or years),
  next payment date, category, payment method, website icon and notes
- Monthly and yearly totals, with every currency converted to your main currency
  (exchange rates refreshed once a day)
- Calendar of upcoming payments
- Statistics: cost by category and by payment method, share of total, next 30 days
- Windows notifications before a renewal (you choose how many days before, per subscription)
- Tray icon with a `+N` badge for renewals you have not looked at yet
- Search, filters and sorting
- Light and dark theme (follows Windows by default)
- Starts with Windows (can be turned off)
- Export to CSV

## How it stays light

| | RAM | Disk |
|---|---|---|
| In the tray (window closed) | ~2 MB | 0.6 MB exe |
| Window open | +~180 MB (WebView2), freed on close | — |

- The tray part is plain Win32 written in Rust, with no runtime.
- The window is drawn by the WebView2 runtime that is part of Windows 11. It runs in a
  separate short-lived process, so closing the window gives all of its memory back.
- Data is stored as small text files next to the exe: `subscriptions.tsv`, `settings.ini`,
  `rates.tsv` and `state.tsv`. Copy the folder to back it up.

## Install

1. Download `Subscribity.exe` from the releases page and put it in a folder of its own
   (for example `C:\Users\<you>\Subscribity`). Your data is saved in that same folder.
2. Run it. The window opens the first time; after that, click the tray icon.

Needs Windows 10 or 11 with the Microsoft Edge WebView2 Runtime (already installed on Windows 11).

## Updates

Subscribity checks GitHub Releases once a week (or once a month, or never; see Settings).
When a newer version exists you get a Windows notification and an **Update now** button in
the app. The update is downloaded, its SHA-256 is checked against the published checksum,
the exe is swapped in place and the app restarts. Your data is not touched.

## Releasing a new version

1. Bump `version` in `Cargo.toml` (for example `0.2.0`) and commit.
2. Tag and push:

   ```powershell
   git tag v0.2.0
   git push origin main v0.2.0
   ```

3. The **Release** GitHub Action tests and builds the app and publishes a release with
   `Subscribity.exe` and `Subscribity.exe.sha256`. Installed apps pick it up at their next check,
   or right away with **Settings → Updates → Check now**.

## Build from source

```powershell
# Rust (https://rustup.rs) and the MSVC build tools (Visual Studio Build Tools, C++ workload)
cargo build --release
```

The exe is written to `target\x86_64-pc-windows-msvc\release\subscribity.exe`.

Run the tests with `cargo test --release`.

## Privacy

Subscribity has no account and no telemetry. It makes two kinds of network requests:

- exchange rates from `open.er-api.com`, once a day, only if you use more than one currency
- website icons from `icons.duckduckgo.com`, only for subscriptions where you entered a website

## Credits

Inspired by [Wallos](https://github.com/ellite/Wallos), the open-source self-hosted subscription
tracker. Subscribity is an independent app written from scratch and shares no code with it.

## License

MIT, see [LICENSE](LICENSE).
