//! The one family-health probe, ported from im's `health.rs`: a sibling is
//! asked `GET {url}/healthz` where it stands — no credentials, two seconds
//! to answer — and [`probe_family`] fans the probes out concurrently, so a
//! family member that is down costs its two seconds, not two seconds each.
//! The readings are cached for [`PROBE_TTL`], so a page render inside the
//! window reuses the last round instead of stalling on every sibling
//! again. iz has no admin health table: the wordmark's flyout is the only
//! reader, so the probe's body and latency ride the enum ([`Probe::Up`])
//! but only the on/off dot ever renders.

/// One `/healthz` reading. `Up` carries the body — the deploy contract's
/// `ok <build sha>` — and the answer's latency; everything else, refused
/// or wrong status or a body that does not begin ok or the two-second
/// ceiling, is `Down`.
#[derive(Clone)]
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

/// One shared client for the app's lifetime: a fresh pool per render would
/// tear down and re-handshake every socket it touched.
static PROBE_HTTP: std::sync::LazyLock<reqwest::Client> =
    std::sync::LazyLock::new(reqwest::Client::new);

/// The last family-wide round of readings and when it landed, keyed by the
/// probed urls: a family that changed shape misses the cache and probes
/// fresh instead of wearing a stranger's dot.
static PROBES: parking_lot::Mutex<Option<(std::time::Instant, Vec<(String, Probe)>)>> =
    parking_lot::Mutex::new(None);

/// How long one round of readings stands in for the live siblings.
const PROBE_TTL: std::time::Duration = std::time::Duration::from_secs(30);

/// Every url probed at once — a family member that is down costs its two
/// seconds, not two seconds each — but at most one round per [`PROBE_TTL`]:
/// a render inside the window reuses the last reading instead of stalling
/// on every sibling again.
pub(crate) async fn probe_family(urls: Vec<String>) -> Vec<Probe> {
    {
        let cache = PROBES.lock();
        if let Some((at, readings)) = cache.as_ref() {
            let same = readings
                .iter()
                .map(|(url, _)| url.as_str())
                .eq(urls.iter().map(String::as_str));
            if at.elapsed() < PROBE_TTL && same {
                return readings.iter().map(|(_, probe)| probe.clone()).collect();
            }
        }
    }
    let mut probes = Vec::new();
    for url in &urls {
        let http = PROBE_HTTP.clone();
        let url = url.clone();
        probes.push(tokio::spawn(async move { probe_healthz(&http, &url).await }));
    }
    let mut readings: Vec<(String, Probe)> = Vec::with_capacity(urls.len());
    for (url, probe) in urls.into_iter().zip(probes) {
        let probe = probe.await.unwrap_or(Probe::Down);
        readings.push((url, probe));
    }
    *PROBES.lock() = Some((std::time::Instant::now(), readings.clone()));
    readings.into_iter().map(|(_, probe)| probe).collect()
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
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cache is the point of [`probe_family`]: the same family probed
    /// twice inside the window dials the sibling once, and a family that
    /// changed shape misses the cache and dials the newcomer fresh.
    #[tokio::test]
    async fn a_render_inside_the_window_reuses_the_last_round() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::AsyncWriteExt as _;

        async fn stand_in() -> (std::net::SocketAddr, Arc<AtomicUsize>) {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let hits = Arc::new(AtomicUsize::new(0));
            let counted = hits.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((mut socket, _)) = listener.accept().await else {
                        break;
                    };
                    counted.fetch_add(1, Ordering::SeqCst);
                    let _ = socket
                        .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok")
                        .await;
                }
            });
            (addr, hits)
        }

        let (a, a_hits) = stand_in().await;
        let (b, b_hits) = stand_in().await;
        let urls = |addr: std::net::SocketAddr| vec![format!("http://{addr}/healthz")];
        probe_family(urls(a)).await;
        probe_family(urls(a)).await;
        assert_eq!(
            a_hits.load(Ordering::SeqCst),
            1,
            "the second render inside the window dialled the sibling again"
        );
        probe_family(urls(b)).await;
        assert_eq!(
            b_hits.load(Ordering::SeqCst),
            1,
            "a changed family did not probe the newcomer fresh"
        );
    }
}
