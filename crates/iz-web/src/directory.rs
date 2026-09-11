//! How the identity mirror stands against im. Two halves: the facts the
//! Settings Connection card renders (the mirror task writes, the page
//! reads), and the live connection itself — held and parsed here rather
//! than through the pinned `im-client`, whose frame reader drops every
//! event kind it was not built for, and the feed now carries kinds this
//! app must hear.
//!
//! Deliberately dumb: one mutex over four plain fields, because the card
//! renders once per view and the stream updates a few times a minute. The
//! client id and issuer are public by OIDC design; the client secret never
//! comes near this struct, so the render path cannot leak what it was never
//! given.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use iz_core::Topic;
use iz_core::store::Store;
use parking_lot::Mutex;
use serde::Deserialize;
use topcoat::context::{Cx, try_app_context};

/// The mirror's health, shared between the stream task and the renderer.
#[derive(Clone, Default)]
pub struct DirectoryHealth(Arc<Mutex<Inner>>);

#[derive(Default)]
struct Inner {
    /// The `/directory/live` stream is open.
    connected: bool,
    opened_at: Option<Instant>,
    event_at: Option<Instant>,
    pass_at: Option<Instant>,
}

/// One rendered view of [`DirectoryHealth`]: the state word the card shows
/// and the ages behind the two quiet facts.
#[derive(Clone, Copy)]
pub struct Snapshot {
    pub connected: bool,
    pub event_age: Option<Duration>,
    pub pass_age: Option<Duration>,
}

impl DirectoryHealth {
    pub fn new() -> Self {
        Self::default()
    }

    /// The stream opened (or reopened): the card may say Connected.
    pub fn connected(&self) {
        let mut inner = self.0.lock();
        inner.connected = true;
        inner.opened_at = Some(Instant::now());
    }

    /// The stream ended or im refused it: the card says Reconnecting until
    /// the next full pass opens a fresh one.
    pub fn reconnecting(&self) {
        self.0.lock().connected = false;
    }

    /// A Profile event came off the stream.
    pub fn event(&self) {
        self.0.lock().event_at = Some(Instant::now());
    }

    /// A full roster pass finished — the stream's boot replay, a resync
    /// after a drop, or the watchdog beat's pass.
    pub fn pass(&self) {
        self.0.lock().pass_at = Some(Instant::now());
    }

    pub fn snapshot(&self) -> Snapshot {
        let inner = self.0.lock();
        let now = Instant::now();
        Snapshot {
            connected: inner.connected,
            event_age: inner.event_at.map(|at| now.saturating_duration_since(at)),
            pass_age: inner.pass_at.map(|at| now.saturating_duration_since(at)),
        }
    }
}

/// The health `main.rs` registers on the router beside the directory
/// client it describes.
pub fn health(cx: &Cx) -> &DirectoryHealth {
    try_app_context::<DirectoryHealth>(cx)
        .expect("the directory health was registered on the router")
}

/// The stream's age in whole seconds, as the card prints it — a short
/// number with its unit, the same shape in every language.
pub fn age_text(age: Option<Duration>) -> String {
    match age {
        Some(age) => {
            let secs = age.as_secs();
            if secs >= 60 {
                format!("{}m", secs / 60)
            } else {
                format!("{secs}s")
            }
        }
        None => String::new(),
    }
}

/// Where the live half of the mirror connects. The same issuer and Basic
/// pair the roster client holds, kept apart from it because the stream is
/// read here, frame by frame, rather than through the pinned `im-client` —
/// whose parser drops every event kind it was not built for, and this feed
/// now carries kinds this app must hear.
#[derive(Clone)]
pub struct StreamSource {
    http: reqwest::Client,
    issuer: String,
    client_id: String,
    client_secret: String,
}

impl StreamSource {
    pub fn new(
        issuer: impl Into<String>,
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
    ) -> Self {
        Self {
            http: reqwest::Client::new(),
            issuer: issuer.into(),
            client_id: client_id.into(),
            client_secret: client_secret.into(),
        }
    }

    /// `GET /directory/live`, the connection held with no timeout: a change
    /// feed is *supposed* to sit idle, and its liveness is im's keep-alive
    /// comment.
    pub async fn open_stream(&self) -> Result<DirectoryStream, String> {
        let response = self
            .http
            .get(format!("{}/directory/live", self.issuer))
            .basic_auth(&self.client_id, Some(&self.client_secret))
            .send()
            .await
            .map_err(|e| e.to_string())?
            .error_for_status()
            .map_err(|e| e.to_string())?;
        Ok(DirectoryStream {
            response,
            reader: FrameReader::default(),
        })
    }
}

/// One member's row exactly as the stream carries it: the roster's shape
/// plus the `disabled` fact the roster itself never shows — im's
/// `/directory` lists nobody disabled, so the flag travels only here.
/// Unknown fields are ignored, so an im newer than this build can grow the
/// row without a breaking change.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct StreamMember {
    pub sub: String,
    pub email: String,
    pub name: String,
    #[serde(default)]
    pub admin: bool,
    #[serde(default)]
    pub photo_version: u64,
    #[serde(default = "default_timezone")]
    pub timezone: String,
    #[serde(default)]
    pub disabled: bool,
}

/// The timezone im starts every account on, and the one a mirrored row
/// predating the field reads as.
fn default_timezone() -> String {
    "UTC+03:00".to_string()
}

/// One event off im's `/directory/live`, parsed here rather than in the
/// pinned client because the consumer owes the wire more than the pin
/// knows: a `revoked` frame says a session died in im, and a `profile`
/// frame may now carry `disabled`.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamFrame {
    /// The named member's row as it now stands.
    Profile(StreamMember),
    /// At least one of the member's sessions was revoked. Which one is
    /// im's knowledge alone; the consumer treats the fact workspace-wide.
    Revoked { sub: String },
}

/// An open `/directory/live` connection. `next` mirrors the pinned
/// client's shape: `Some(Err)` on a wire failure — then the stream is
/// over — and `None` when im closed a healthy one. Either way the caller
/// re-lists and redials.
pub struct DirectoryStream {
    response: reqwest::Response,
    reader: FrameReader,
}

impl DirectoryStream {
    pub async fn next(&mut self) -> Option<Result<StreamFrame, String>> {
        loop {
            if let Some(frame) = self.reader.ready.pop_front() {
                return Some(Ok(frame));
            }
            match self.response.chunk().await {
                Ok(Some(bytes)) => self.reader.feed(&bytes),
                Ok(None) => return None,
                Err(e) => return Some(Err(e.to_string())),
            }
        }
    }
}

/// The hand-rolled `text/event-stream` reader. Frames accumulate field by
/// field and dispatch on the blank line, per the SSE grammar; the fields
/// this feed carries are `event`, `data`, and comments, but the parser
/// honors the whole shape — including `data` split over several lines,
/// which the spec joins with `\n`.
#[derive(Default)]
struct FrameReader {
    /// Bytes read but not yet through a line terminator.
    pending: Vec<u8>,
    /// The previous read ended on a bare `\r`, so the next read's leading
    /// `\n` — if that is what comes — completes that terminator rather
    /// than opening a blank line.
    cr_ended: bool,
    /// The current frame's `event:` field, if it named one.
    event: Option<String>,
    /// The current frame's `data:`, joined back into one string.
    data: String,
    /// Frames completed and waiting to be read.
    ready: VecDeque<StreamFrame>,
}

impl FrameReader {
    /// Feeds the next bytes from the wire and queues every frame they
    /// completed. Bytes trailing their terminator wait for the rest — a
    /// field or a UTF-8 character may straddle two reads.
    fn feed(&mut self, bytes: &[u8]) {
        // The previous read stopped on a bare `\r`. A `\r\n` split across
        // two reads is ONE terminator: the pair's `\n` arriving here
        // completes the old one instead of opening a blank line and
        // dispatching a frame that has not ended.
        let bytes = if self.cr_ended {
            self.cr_ended = false;
            if bytes.first() == Some(&b'\n') {
                &bytes[1..]
            } else {
                bytes
            }
        } else {
            bytes
        };
        self.pending.extend_from_slice(bytes);
        let Some(at) = self
            .pending
            .iter()
            .rposition(|&b| b == b'\n' || b == b'\r')
            .map(|at| at + 1)
        else {
            return;
        };
        let block: Vec<u8> = self.pending.drain(..at).collect();
        self.cr_ended = block.last() == Some(&b'\r');
        for line in split_lines(&block) {
            if line.is_empty() {
                // The blank line dispatches whatever accumulated.
                if let Some(frame) = self.take() {
                    self.ready.push_back(frame);
                }
            } else if line[0] == b':' {
                // A comment — im's keep-alives and retry hint live here.
            } else {
                let (name, value) = match line.iter().position(|&b| b == b':') {
                    Some(at) => (&line[..at], &line[at + 1..]),
                    None => (&line[..], &b""[..]),
                };
                // One optional space after the colon is not part of the
                // value.
                let value = value.strip_prefix(b" ").unwrap_or(value);
                match name {
                    b"event" => self.event = Some(String::from_utf8_lossy(value).into_owned()),
                    b"data" => {
                        if !self.data.is_empty() {
                            self.data.push('\n');
                        }
                        self.data.push_str(&String::from_utf8_lossy(value));
                    }
                    // `id` and `retry` mean nothing to a change feed.
                    _ => {}
                }
            }
        }
    }

    /// Ends the current frame and decides what it was. A frame whose kind
    /// this build does not know is skipped, not fatal: im may grow the
    /// feed, and one strange frame must not end a healthy connection. A
    /// known kind whose payload will not parse is logged and dropped for
    /// the same reason.
    fn take(&mut self) -> Option<StreamFrame> {
        let data = std::mem::take(&mut self.data);
        let event = self.event.take();
        match event.as_deref() {
            // A frame with no kind at all is an older im's bare member
            // announcement; it parses exactly as it always did.
            None | Some("profile") => match serde_json::from_str::<StreamMember>(&data) {
                Ok(member) => Some(StreamFrame::Profile(member)),
                Err(problem) => {
                    eprintln!("directory stream: unreadable profile frame ({problem})");
                    None
                }
            },
            Some("revoked") => match serde_json::from_str::<RevokedFrame>(&data) {
                Ok(frame) => Some(StreamFrame::Revoked { sub: frame.sub }),
                Err(problem) => {
                    eprintln!("directory stream: unreadable revoked frame ({problem})");
                    None
                }
            },
            // A kind from an im newer than this build.
            Some(_) => None,
        }
    }
}

/// The `{"sub": ...}` payload a `revoked` frame carries.
#[derive(Deserialize)]
struct RevokedFrame {
    sub: String,
}

/// Splits a block that ends at a line terminator into lines, accepting the
/// three SSE terminators: `\n`, `\r\n`, and a bare `\r`. A blank line is a
/// frame's dispatch, so it is kept — only the terminator run itself
/// disappears.
fn split_lines(block: &[u8]) -> Vec<Vec<u8>> {
    let mut lines = Vec::new();
    let mut start = 0usize;
    let mut at = 0usize;
    while at < block.len() {
        if block[at] == b'\n' || block[at] == b'\r' {
            lines.push(block[start..at].to_vec());
            if block[at] == b'\r' && block.get(at + 1) == Some(&b'\n') {
                at += 1;
            }
            at += 1;
            start = at;
        } else {
            at += 1;
        }
    }
    if start < block.len() {
        lines.push(block[start..].to_vec());
    }
    lines
}

/// Carries one consumed frame into the local mirror — the one door both
/// the stream task and the tests use, so a frame means the same thing no
/// matter who fed it.
///
/// * A profile syncs the row the way the roster pass does, and im owns the
///   account's life: a profile whose `disabled` disagrees with the local
///   row flips it through the same store path an admin's hand uses —
///   `true` is a user-level kill whose store announcement names the
///   account's own open tabs, `false` is the homecoming that signs the
///   row back in.
/// * A revoked frame is a session fact — which session died is im's
///   knowledge alone, and a tab whose *other* session survived must not be
///   sent away — so it only announces the member surface. Every tab's
///   refetch re-introspects per request, and that is what sorts out who is
///   still signed in.
pub async fn apply_frame(store: &Arc<dyn Store>, frame: StreamFrame) {
    match frame {
        StreamFrame::Profile(member) => {
            if let Err(problem) = store
                .sync_member(
                    &member.sub,
                    &member.email,
                    &member.name,
                    member.admin,
                    member.photo_version,
                    &member.timezone,
                )
                .await
            {
                eprintln!("directory stream: {}: {problem}", member.email);
            }
            match store.user_by_sub(&member.sub).await {
                Ok(Some(user)) if user.disabled != member.disabled => {
                    if let Err(problem) = store.set_user_disabled(&user.id, member.disabled).await
                    {
                        eprintln!("directory stream: {}: {problem}", member.email);
                    }
                }
                Ok(_) => {}
                Err(problem) => {
                    eprintln!("directory stream: {}: {problem}", member.email);
                }
            }
        }
        StreamFrame::Revoked { .. } => {
            store.announce(&[Topic::Members]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(wire: &str) -> Vec<StreamFrame> {
        let mut reader = FrameReader::default();
        reader.feed(wire.as_bytes());
        reader.ready.drain(..).collect()
    }

    fn member(sub: &str, email: &str, name: &str, disabled: bool) -> StreamMember {
        StreamMember {
            sub: sub.into(),
            email: email.into(),
            name: name.into(),
            admin: false,
            photo_version: 0,
            timezone: "UTC+03:00".into(),
            disabled,
        }
    }

    /// A kind this build has never heard of is skipped where it stands —
    /// the frames on either side of it still land, and the reader is alive
    /// for the next bytes.
    #[test]
    fn an_unknown_event_kind_is_skipped_and_the_stream_lives_on() {
        let wire = concat!(
            ": keep-alive\n",
            "event: profile\n",
            "data: {\"sub\":\"s1\",\"email\":\"a@iz.sh\",\"name\":\"A\"}\n",
            "\n",
            "event: logrollover\n",
            "data: {\"whatever\":true}\n",
            "\n",
            "event: revoked\n",
            "data: {\"sub\":\"s1\"}\n",
            "\n",
            "event: profile\n",
            "data: {\"sub\":\"s2\",\"email\":\"b@iz.sh\",\"name\":\"B\",\"disabled\":true}\n",
            "\n",
        );
        assert_eq!(
            frames(wire),
            vec![
                StreamFrame::Profile(member("s1", "a@iz.sh", "A", false)),
                StreamFrame::Revoked { sub: "s1".into() },
                StreamFrame::Profile(member("s2", "b@iz.sh", "B", true)),
            ]
        );
    }

    /// im predating the `event:` field altogether announces bare member
    /// frames; unknown fields on a known frame are ignored, so a newer im
    /// may grow the row without a breaking change.
    #[test]
    fn a_bare_frame_from_an_older_im_still_parses() {
        let wire = concat!(
            "data: {\"sub\":\"s1\",\"email\":\"a@iz.sh\",\"name\":\"A\",\"voltage\":9}\n",
            "\n",
        );
        let seen = frames(wire);
        assert_eq!(
            seen,
            vec![StreamFrame::Profile(member("s1", "a@iz.sh", "A", false))]
        );
    }

    /// A frame cut in the middle of its JSON is held until its rest
    /// arrives; nothing is lost and nothing is dispatched early.
    #[test]
    fn a_frame_straddling_two_reads_is_not_lost() {
        let mut reader = FrameReader::default();
        reader.feed(b"event: revoked\ndata: {\"su");
        assert!(reader.ready.is_empty());
        reader.feed(b"b\":\"s9\"}\n\n");
        assert_eq!(
            reader.ready.drain(..).collect::<Vec<_>>(),
            vec![StreamFrame::Revoked { sub: "s9".into() }]
        );
    }

    /// A `\r\n` pair split across two reads stays one terminator: the
    /// trailing `\r` of a read must not let the next read's leading `\n`
    /// arrive as a blank line and dispatch a frame that has not ended.
    /// The cut here is exactly at the terminator of the one data line;
    /// the blank line that follows in the next read is what dispatches.
    #[test]
    fn a_crlf_pair_split_across_reads_is_one_terminator() {
        let mut reader = FrameReader::default();
        reader.feed(b"data: {\"sub\":\"s1\",\"email\":\"a@iz.sh\",\"name\":\"A\"}\r");
        assert!(reader.ready.is_empty(), "a cut terminator dispatched early");
        reader.feed(b"\n\r\n");
        assert_eq!(
            reader.ready.drain(..).collect::<Vec<_>>(),
            vec![StreamFrame::Profile(member("s1", "a@iz.sh", "A", false))]
        );
    }

    /// The same hold with a named kind: the `\r` after `event: revoked` is
    /// a field line's end, not the frame's — draining it would let the
    /// next read open with a blank line and burn the frame on an empty
    /// payload, losing the revocation for good.
    #[test]
    fn a_trailing_cr_after_the_event_field_does_not_burn_the_frame() {
        let mut reader = FrameReader::default();
        reader.feed(b"event: revoked\r");
        assert!(reader.ready.is_empty());
        reader.feed(b"\ndata: {\"sub\":\"s9\"}\r\n\r\n");
        assert_eq!(
            reader.ready.drain(..).collect::<Vec<_>>(),
            vec![StreamFrame::Revoked { sub: "s9".into() }]
        );
    }
}
