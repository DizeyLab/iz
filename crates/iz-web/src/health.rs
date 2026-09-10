//! The one family-health probe, ported from im's `health.rs`: a sibling is
//! asked `GET {url}/healthz` where it stands — no credentials, two seconds
//! to answer — and the callers fan the probes out concurrently, so a family
//! member that is down costs its two seconds, not two seconds each. iz has
//! no admin health table: the wordmark's flyout is the only reader, so the
//! probe's body and latency ride the enum ([`Probe::Up`]) but only the
//! on/off dot ever renders.

/// One `/healthz` reading. `Up` carries the body — the deploy contract's
/// `ok <build sha>` — and the answer's latency; everything else, refused
/// or wrong status or a body that does not begin ok or the two-second
/// ceiling, is `Down`.
pub enum Probe {
    Up { body: String, ms: u128 },
    Down,
}

pub async fn probe_healthz(http: &reqwest::Client, url: &str) -> Probe {
    let started = std::time::Instant::now();
    let Ok(answer) = http
        .get(url)
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .await
    else {
        return Probe::Down;
    };
    if !answer.status().is_success() {
        return Probe::Down;
    }
    let body = answer.text().await.unwrap_or_default();
    let body = body.trim();
    if body.starts_with("ok") {
        Probe::Up {
            body: body.chars().take(64).collect(),
            ms: started.elapsed().as_millis(),
        }
    } else {
        Probe::Down
    }
}

/// The flyout's marks as final HTML, pure over resolved probes: this app's
/// own row filtered out, every sibling a plain link carrying its probe's
/// dot — `health-on` while it answers `ok`, `health-off` while it does not
/// — middots between. Dots only: the probe's body and latency are the
/// admin table's business, and iz draws no admin table. Extracted from
/// `family_mark` so the tests pin this exact markup without a router or a
/// live sibling.
pub fn switcher_marks<'a>(
    rows: impl IntoIterator<Item = (&'a str, &'a str, &'a str, Probe)>,
) -> String {
    rows.into_iter()
        .filter(|(key, _, _, _)| *key != "iz")
        .map(|(key, name, url, probe)| {
            let key = escape(key);
            let name = escape(name);
            let url = escape(url);
            let dot = match probe {
                Probe::Up { .. } => "health-on",
                Probe::Down => "health-off",
            };
            format!(
                r#"<a class="service-mark" href="{url}" title="{name}" data-hard=""><span class="health-dot {dot}"></span>{key}</a>"#
            )
        })
        .collect::<Vec<_>>()
        .join(r#"<span class="service-sep">·</span>"#)
}

/// The switcher builds its marks as a string — one per sibling — so its
/// interpolations never pass through `view!`'s escaping. Everything the
/// mirror carries (key, name, url) is im-admin-controlled, and it crosses
/// here before it reaches the markup.
fn escape(raw: &str) -> String {
    raw.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
