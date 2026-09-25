//! Streaming pre-scanner for the escape sequences Amalgum honours beyond alacritty's set
//! (§5.27). The PTY reader feeds every chunk through [`Scanner::feed`] *and* on to the
//! terminal parser unchanged; the scanner only observes.
//!
//! Recognised (terminated by BEL `\x07` or ST `ESC \`; sequences may be split across chunks):
//! - OSC 7 `file://host/path` → [`OscEvent::Cwd`] (path percent-decoded)
//! - OSC 0 / OSC 2 → [`OscEvent::Title`]
//! - OSC 9 `<body>` → Notify without title. `9;4;…` is ConEmu progress: ignored.
//! - OSC 99 `<metadata>;<payload>` (kitty) → Notify. The metadata picks the payload's part
//!   (`p=title`, the default, or `p=body`; queries and other parts are ignored), its encoding
//!   (`e=1`: base64), and chunking (`d=0`: more chunks with the same `i=` follow). A lone title
//!   becomes the body, as with OSC 9.
//! - OSC 777 `notify;<title>;<body>` → Notify
//! - OSC 133 `A` / `B` / `C` / `D[;exit]` → [`OscEvent::Prompt`]
//! - a bare BEL outside any sequence → [`OscEvent::Bell`]
//!
//! Anything longer than [`MAX_OSC`] bytes is discarded (never buffered without bound).
//! Invalid UTF-8 is replaced lossily.
//!
//! The 8-bit C1 forms (`0x9d` OSC, `0x9c` ST) are deliberately *not* recognised: PTY output is
//! UTF-8, where those bytes are continuation bytes of ordinary characters (`✳`, `✅`, `”`), and
//! alacritty's parser does not treat them as controls either.

pub const MAX_OSC: usize = 8192;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OscEvent {
    Cwd { host: Option<String>, path: String },
    Title(String),
    Notify { title: Option<String>, body: String },
    Prompt(PromptMark),
    Bell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptMark {
    /// `A`: prompt shown → idle.
    PromptStart,
    /// `B`: user is typing a command.
    CommandStart,
    /// `C`: command running.
    CommandExecuted,
    /// `D[;exit]`: command finished; non-zero exit shows a one-shot warning dot.
    CommandFinished { exit: Option<i32> },
}

/// Byte-level scanner state. `Esc` means "just saw ESC (0x1b) in Ground"; `OscEsc` means "just
/// saw ESC while inside an OSC payload, awaiting `\` to complete the ST terminator".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum State {
    #[default]
    Ground,
    Esc,
    Osc,
    OscEsc,
}

const BEL: u8 = 0x07;
const ESC: u8 = 0x1b;

#[derive(Debug, Default)]
pub struct Scanner {
    state: State,
    buf: Vec<u8>,
    /// Set once `buf` would exceed [`MAX_OSC`]; further bytes for this sequence are discarded
    /// and no event is emitted when it terminates.
    overflowed: bool,
    /// A kitty OSC 99 notification whose last chunk has not arrived yet.
    kitty: Option<KittyNotification>,
}

/// The text of OSC 99 chunks sharing one `i=` id, each part capped at [`MAX_OSC`] bytes.
#[derive(Debug, Default)]
struct KittyNotification {
    id: String,
    title: String,
    body: String,
}

impl Scanner {
    /// Scan `bytes`, appending recognised events to `out` in stream order. A sequence split
    /// across two calls (e.g. `ESC` in one chunk, `]0;title` `BEL` in the next) is still
    /// recognised, since all state lives in `self`.
    pub fn feed(&mut self, bytes: &[u8], out: &mut Vec<OscEvent>) {
        for &b in bytes {
            self.step(b, out);
        }
    }

    fn step(&mut self, b: u8, out: &mut Vec<OscEvent>) {
        match self.state {
            State::Ground => match b {
                ESC => self.state = State::Esc,
                BEL => out.push(OscEvent::Bell),
                _ => {}
            },
            State::Esc => match b {
                b']' => self.start_osc(),
                // Anything else (including a stray `\`) is not an OSC introducer: the CSI/other
                // sequence passes through untouched, we just stop watching it.
                _ => self.state = State::Ground,
            },
            State::Osc => match b {
                BEL => self.finish(out),
                ESC => self.state = State::OscEsc,
                _ => self.push_byte(b),
            },
            State::OscEsc => match b {
                b'\\' => self.finish(out),
                // Not a valid ST: abandon this OSC (no event) and let `]` start a fresh one,
                // matching Ground/Esc's "anything but `]`/`\` returns to Ground" rule.
                b']' => self.start_osc(),
                _ => self.state = State::Ground,
            },
        }
    }

    fn start_osc(&mut self) {
        self.buf.clear();
        self.overflowed = false;
        self.state = State::Osc;
    }

    fn push_byte(&mut self, b: u8) {
        if self.buf.len() < MAX_OSC {
            self.buf.push(b);
        } else {
            self.overflowed = true;
        }
    }

    fn finish(&mut self, out: &mut Vec<OscEvent>) {
        if !self.overflowed {
            let payload = String::from_utf8_lossy(&self.buf);
            let ev = match payload.split_once(';') {
                Some(("99", rest)) => parse_99(rest, &mut self.kitty),
                _ => parse_payload(&payload),
            };
            if let Some(ev) = ev {
                out.push(ev);
            }
        }
        self.buf.clear();
        self.overflowed = false;
        self.state = State::Ground;
    }
}

/// Parse one complete OSC payload (the bytes between the introducer and the terminator), except
/// OSC 99, whose chunks need the scanner's state ([`parse_99`]).
fn parse_payload(payload: &str) -> Option<OscEvent> {
    let (code, rest) = payload.split_once(';').unwrap_or((payload, ""));
    match code {
        "0" | "2" => Some(OscEvent::Title(rest.to_string())),
        "7" => parse_cwd(rest),
        "9" => {
            // `9;4;…` is a ConEmu progress report, not a notification.
            if rest.starts_with("4;") {
                None
            } else {
                Some(OscEvent::Notify { title: None, body: rest.to_string() })
            }
        }
        "777" => parse_777(rest),
        "133" => parse_133(rest),
        _ => None,
    }
}

/// Kitty OSC 99 `<metadata>;<payload>`, the metadata being `key=value` pairs joined by `:`. Adds
/// this chunk to `pending` and returns the notification once its last chunk (`d=1`, the default)
/// arrives. Only the `title` (default) and `body` parts carry text; `?` queries, `close`,
/// `alive`, `icon`, and `buttons` don't. A lone title becomes the body, as with OSC 9.
fn parse_99(rest: &str, pending: &mut Option<KittyNotification>) -> Option<OscEvent> {
    let (metadata, payload) = rest.split_once(';').unwrap_or((rest, ""));
    let (mut id, mut done, mut part, mut base64) = ("", true, "title", false);
    for (key, value) in metadata.split(':').filter_map(|kv| kv.split_once('=')) {
        match key {
            "i" => id = value,
            "d" => done = value != "0",
            "p" => part = value,
            "e" => base64 = value == "1",
            _ => {}
        }
    }

    let mut n = pending
        .take()
        .filter(|n| n.id == id)
        .unwrap_or_else(|| KittyNotification { id: id.to_string(), ..KittyNotification::default() });
    let field = match part {
        "title" => Some(&mut n.title),
        "body" => Some(&mut n.body),
        _ => None,
    };
    if let Some(field) = field {
        let text = if base64 { decode_base64(payload) } else { payload.to_string() };
        if field.len() + text.len() <= MAX_OSC {
            field.push_str(&text);
        }
    }
    if !done {
        *pending = Some(n);
        return None;
    }
    match (n.title.is_empty(), n.body.is_empty()) {
        (true, true) => None,
        (false, true) => Some(OscEvent::Notify { title: None, body: n.title }),
        (true, false) => Some(OscEvent::Notify { title: None, body: n.body }),
        (false, false) => Some(OscEvent::Notify { title: Some(n.title), body: n.body }),
    }
}

/// Decode standard base64 (padding optional; bytes outside the alphabet are skipped) as UTF-8,
/// replacing invalid sequences lossily.
fn decode_base64(input: &str) -> String {
    let mut out = Vec::with_capacity(input.len() / 4 * 3 + 2);
    let (mut acc, mut bits) = (0u32, 0u32);
    for b in input.bytes() {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => continue,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_cwd(rest: &str) -> Option<OscEvent> {
    let body = rest.strip_prefix("file://").or_else(|| rest.strip_prefix("kitty-shell-cwd://"))?;
    let (host, path) = match body.split_once('/') {
        Some((h, p)) => (h, format!("/{p}")),
        None => (body, String::new()),
    };
    let host = if host.is_empty() { None } else { Some(host.to_string()) };
    Some(OscEvent::Cwd { host, path: percent_decode(&path) })
}

/// Percent-decode a path. An invalid escape (`%` not followed by two hex digits) is left as a
/// literal `%` rather than dropped or treated as an error.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 3 <= bytes.len()
            && let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push(hi * 16 + lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// `notify;title;body`; body may itself contain `;`, so only the first two separators split.
fn parse_777(rest: &str) -> Option<OscEvent> {
    let mut parts = rest.splitn(3, ';');
    if parts.next()? != "notify" {
        return None;
    }
    let title = parts.next()?.to_string();
    let body = parts.next().unwrap_or("").to_string();
    Some(OscEvent::Notify { title: Some(title), body })
}

/// `A` / `B` / `C` / `D[;exit]`, with any further `;k=v` params ignored.
fn parse_133(rest: &str) -> Option<OscEvent> {
    let mut parts = rest.split(';');
    let mark = match parts.next()? {
        "A" => PromptMark::PromptStart,
        "B" => PromptMark::CommandStart,
        "C" => PromptMark::CommandExecuted,
        "D" => {
            let exit = parts.next().and_then(|s| s.parse::<i32>().ok());
            PromptMark::CommandFinished { exit }
        }
        _ => return None,
    };
    Some(OscEvent::Prompt(mark))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all(bytes: &[u8]) -> Vec<OscEvent> {
        let mut scanner = Scanner::default();
        let mut out = Vec::new();
        scanner.feed(bytes, &mut out);
        out
    }

    fn feed_byte_by_byte(bytes: &[u8]) -> Vec<OscEvent> {
        let mut scanner = Scanner::default();
        let mut out = Vec::new();
        for &b in bytes {
            scanner.feed(&[b], &mut out);
        }
        out
    }

    /// Deterministic (seeded xorshift64) chunk boundaries, 1..=7 bytes per chunk, so split-chunk
    /// tests are reproducible without a `rand` dependency.
    fn feed_deterministic_splits(bytes: &[u8], seed: u64) -> Vec<OscEvent> {
        let mut scanner = Scanner::default();
        let mut out = Vec::new();
        let mut state = seed | 1; // xorshift needs a non-zero state
        let mut pos = 0;
        while pos < bytes.len() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let step = 1 + (state % 7) as usize;
            let end = (pos + step).min(bytes.len());
            scanner.feed(&bytes[pos..end], &mut out);
            pos = end;
        }
        out
    }

    // -- OSC 7 (cwd) --------------------------------------------------------------------

    #[test]
    fn osc7_cwd_with_host_and_percent_decoding() {
        let seq = b"\x1b]7;file://host/path%20x\x07";
        assert_eq!(feed_all(seq), vec![OscEvent::Cwd { host: Some("host".into()), path: "/path x".into() }]);
    }

    #[test]
    fn osc7_empty_host_is_none() {
        let seq = b"\x1b]7;file:///a/b\x07";
        assert_eq!(feed_all(seq), vec![OscEvent::Cwd { host: None, path: "/a/b".into() }]);
    }

    #[test]
    fn osc7_kitty_shell_cwd_scheme_accepted() {
        let seq = b"\x1b]7;kitty-shell-cwd://box/home/me\x07"; // portability: allow
        assert_eq!(
            feed_all(seq),
            vec![OscEvent::Cwd { host: Some("box".into()), path: "/home/me".into() }] // portability: allow
        );
    }

    #[test]
    fn osc7_invalid_percent_escape_left_literal() {
        let seq = b"\x1b]7;file://h/a%zzb\x07";
        assert_eq!(feed_all(seq), vec![OscEvent::Cwd { host: Some("h".into()), path: "/a%zzb".into() }]);
    }

    // -- OSC 0 / 2 (title) --------------------------------------------------------------

    #[test]
    fn osc0_and_osc2_are_title() {
        assert_eq!(feed_all(b"\x1b]0;hello\x07"), vec![OscEvent::Title("hello".into())]);
        assert_eq!(feed_all(b"\x1b]2;world\x07"), vec![OscEvent::Title("world".into())]);
    }

    // -- OSC 9 / 99 / 777 (notify) --------------------------------------------------------

    #[test]
    fn osc9_body_is_notify_without_title() {
        assert_eq!(
            feed_all(b"\x1b]9;build finished\x07"),
            vec![OscEvent::Notify { title: None, body: "build finished".into() }]
        );
    }

    #[test]
    fn osc9_conemu_progress_is_ignored() {
        assert_eq!(feed_all(b"\x1b]9;4;50\x07"), vec![]);
    }

    #[test]
    fn osc99_text_is_the_payload_after_the_metadata() {
        // Kitty: `99 ; <metadata> ; <payload>`, the payload being the title unless `p=body`.
        assert_eq!(
            feed_all(b"\x1b]99;;Hello world\x1b\\"),
            vec![OscEvent::Notify { title: None, body: "Hello world".into() }]
        );
        assert_eq!(
            feed_all(b"\x1b]99;i=1:d=1;hello; there\x07"),
            vec![OscEvent::Notify { title: None, body: "hello; there".into() }]
        );
        assert_eq!(
            feed_all(b"\x1b]99;p=body;just a body\x07"),
            vec![OscEvent::Notify { title: None, body: "just a body".into() }]
        );
    }

    #[test]
    fn osc99_chunks_are_joined_into_one_notification_by_id() {
        // `d=0`: more chunks follow for the same `i`; the title and body arrive separately.
        let seq = b"\x1b]99;i=7:d=0:p=title;Build\x1b\\\x1b]99;i=7:d=0:p=body;3 errors,\x1b\\\
                    \x1b]99;i=7:p=body; see log\x1b\\\x1b]99;i=7:d=1:a=focus;\x1b\\";
        let want = vec![OscEvent::Notify { title: Some("Build".into()), body: "3 errors, see log".into() }];
        assert_eq!(feed_all(seq), want);
        for seed in [3u64, 17, 4242] {
            assert_eq!(feed_deterministic_splits(seq, seed), want);
        }
    }

    #[test]
    fn osc99_base64_payload_is_decoded() {
        // `e=1`: base64 UTF-8 ("✅ done" and "Tests").
        assert_eq!(
            feed_all(b"\x1b]99;i=2:d=0:e=1;VGVzdHM=\x1b\\\x1b]99;i=2:e=1:p=body;4pyFIGRvbmU=\x1b\\"),
            vec![OscEvent::Notify { title: Some("Tests".into()), body: "✅ done".into() }]
        );
    }

    #[test]
    fn osc99_queries_and_non_text_payloads_are_not_notifications() {
        assert_eq!(feed_all(b"\x1b]99;i=1:p=?;\x1b\\"), vec![]);
        assert_eq!(feed_all(b"\x1b]99;i=1:p=close;\x1b\\"), vec![]);
        assert_eq!(feed_all(b"\x1b]99;i=1:p=alive;\x1b\\"), vec![]);
        assert_eq!(feed_all(b"\x1b]99;;\x1b\\"), vec![], "no text, no notification");
    }

    #[test]
    fn osc777_notify_with_semicolon_in_body() {
        assert_eq!(
            feed_all(b"\x1b]777;notify;Build done;3 errors; see log\x07"),
            vec![OscEvent::Notify { title: Some("Build done".into()), body: "3 errors; see log".into() }]
        );
    }

    #[test]
    fn osc777_wrong_verb_is_ignored() {
        assert_eq!(feed_all(b"\x1b]777;other;a;b\x07"), vec![]);
    }

    // -- OSC 133 (prompt marks) ----------------------------------------------------------

    #[test]
    fn osc133_all_marks() {
        assert_eq!(feed_all(b"\x1b]133;A\x07"), vec![OscEvent::Prompt(PromptMark::PromptStart)]);
        assert_eq!(feed_all(b"\x1b]133;B\x07"), vec![OscEvent::Prompt(PromptMark::CommandStart)]);
        assert_eq!(feed_all(b"\x1b]133;C\x07"), vec![OscEvent::Prompt(PromptMark::CommandExecuted)]);
        assert_eq!(
            feed_all(b"\x1b]133;D\x07"),
            vec![OscEvent::Prompt(PromptMark::CommandFinished { exit: None })]
        );
        assert_eq!(
            feed_all(b"\x1b]133;D;0\x07"),
            vec![OscEvent::Prompt(PromptMark::CommandFinished { exit: Some(0) })]
        );
        assert_eq!(
            feed_all(b"\x1b]133;D;1\x07"),
            vec![OscEvent::Prompt(PromptMark::CommandFinished { exit: Some(1) })]
        );
    }

    #[test]
    fn osc133_extra_params_ignored() {
        assert_eq!(
            feed_all(b"\x1b]133;C;aid=7;k=v\x07"),
            vec![OscEvent::Prompt(PromptMark::CommandExecuted)]
        );
        assert_eq!(
            feed_all(b"\x1b]133;D;1;err=x\x07"),
            vec![OscEvent::Prompt(PromptMark::CommandFinished { exit: Some(1) })]
        );
    }

    // -- BEL, ST, CSI passthrough ---------------------------------------------------------

    #[test]
    fn bare_bell_in_ground_is_a_bell_event() {
        assert_eq!(feed_all(b"hello\x07world"), vec![OscEvent::Bell]);
    }

    #[test]
    fn bell_terminating_osc_is_not_a_bell() {
        let out = feed_all(b"\x1b]0;t\x07");
        assert_eq!(out, vec![OscEvent::Title("t".into())]);
        assert!(!out.contains(&OscEvent::Bell));
    }

    #[test]
    fn st_terminator_also_ends_an_osc() {
        assert_eq!(feed_all(b"\x1b]0;t\x1b\\"), vec![OscEvent::Title("t".into())]);
    }

    #[test]
    fn utf8_continuation_bytes_are_not_8bit_c1_controls() {
        // ✳ (E2 9C B3) and ✅ (E2 9C 85) carry 0x9c, the 8-bit ST; ” (E2 80 9D) carries 0x9d,
        // the 8-bit OSC introducer. PTY output is UTF-8, so none of them ends or starts an OSC.
        assert_eq!(
            feed_all("\x1b]0;✳ Claude Code\x07".as_bytes()),
            vec![OscEvent::Title("✳ Claude Code".into())]
        );
        assert_eq!(
            feed_all("\x1b]777;notify;Build;✅ tests passed\x07".as_bytes()),
            vec![OscEvent::Notify { title: Some("Build".into()), body: "✅ tests passed".into() }]
        );
        assert_eq!(feed_all("He said “done” now\x07".as_bytes()), vec![OscEvent::Bell]);
    }

    #[test]
    fn bare_8bit_c1_bytes_are_not_controls() {
        // Like alacritty's parser: 0x9d does not open an OSC and 0x9c does not end one.
        assert_eq!(feed_all(b"\x9d0;c1 title\x9c\x07"), vec![OscEvent::Bell]);
        assert_eq!(feed_all(b"\x1b]0;a\x9cb\x07"), vec![OscEvent::Title("a\u{fffd}b".into())]);
    }

    #[test]
    fn csi_sequence_passes_through_without_events() {
        assert_eq!(feed_all(b"\x1b[31mred\x1b[0m"), vec![]);
    }

    #[test]
    fn esc_then_unrelated_byte_returns_to_ground() {
        // ESC followed by something that is neither `]` nor `\`: no event, and a BEL right
        // after is treated as a bare bell again, proving the scanner is back in Ground.
        assert_eq!(feed_all(b"\x1bXsome text\x07"), vec![OscEvent::Bell]);
    }

    #[test]
    fn multiple_sequences_in_one_feed_in_stream_order() {
        let seq = b"\x1b]0;first\x07plain\x07\x1b]133;A\x07";
        assert_eq!(
            feed_all(seq),
            vec![OscEvent::Title("first".into()), OscEvent::Bell, OscEvent::Prompt(PromptMark::PromptStart),]
        );
    }

    // -- Overflow ---------------------------------------------------------------------------

    #[test]
    fn overflowing_osc_emits_nothing_and_caps_the_buffer() {
        let mut scanner = Scanner::default();
        let mut out = Vec::new();
        scanner.feed(b"\x1b]0;", &mut out);
        let filler = vec![b'x'; MAX_OSC + 1000];
        scanner.feed(&filler, &mut out);
        assert!(scanner.buf.len() <= MAX_OSC, "buffer must never exceed MAX_OSC");
        assert!(scanner.overflowed);
        scanner.feed(b"\x07", &mut out);
        assert!(out.is_empty(), "an overflowed sequence emits nothing");
        // The scanner recovers cleanly afterwards.
        scanner.feed(b"\x1b]0;ok\x07", &mut out);
        assert_eq!(out, vec![OscEvent::Title("ok".into())]);
    }

    #[test]
    fn one_megabyte_of_garbage_without_terminator_does_not_grow_past_max_osc() {
        let mut scanner = Scanner::default();
        let mut out = Vec::new();
        scanner.feed(b"\x1b]0;", &mut out);
        let garbage = vec![b'g'; 1024 * 1024];
        scanner.feed(&garbage, &mut out);
        assert!(scanner.buf.len() <= MAX_OSC);
        assert!(scanner.buf.capacity() <= MAX_OSC * 2, "capacity must stay bounded, not track input size");
        assert!(out.is_empty());
    }

    // -- Split-chunk equivalence ------------------------------------------------------------

    #[test]
    fn split_chunks_produce_identical_events() {
        let mut seq = Vec::new();
        seq.extend_from_slice(b"\x1b]7;file://host/some%20path\x07");
        seq.extend_from_slice(b"plain text with a ");
        seq.push(0x07); // bare bell
        seq.extend_from_slice(b" in the middle\x1b[31mred\x1b[0m");
        seq.extend_from_slice(b"\x1b]9;notify body\x07");
        seq.extend_from_slice(b"\x1b]777;notify;title here;body with ; semicolons\x07");
        seq.extend_from_slice(b"\x1b]133;D;1\x07");
        seq.extend_from_slice("\x1b]0;✳ “quoted” title\x07".as_bytes());
        seq.extend_from_slice(b"\x1b]0;final title\x1b\\");

        let whole = feed_all(&seq);
        let per_byte = feed_byte_by_byte(&seq);
        assert_eq!(whole, per_byte, "byte-by-byte must match single feed");
        assert!(!whole.is_empty());

        for seed in [1u64, 2, 42, 1_000_003, 0xdead_beef] {
            let split = feed_deterministic_splits(&seq, seed);
            assert_eq!(whole, split, "seed {seed} split must match single feed");
        }
    }

    #[test]
    fn split_chunks_with_overflow_are_still_consistent() {
        let mut seq = Vec::new();
        seq.extend_from_slice(b"\x1b]0;");
        seq.extend(vec![b'z'; MAX_OSC + 500]);
        seq.push(0x07);
        seq.extend_from_slice(b"\x1b]0;after\x07");

        let whole = feed_all(&seq);
        assert_eq!(whole, vec![OscEvent::Title("after".into())]);
        for seed in [7u64, 99, 12345] {
            assert_eq!(feed_deterministic_splits(&seq, seed), whole);
        }
    }
}
