//! Incremental Server-Sent Events parser.
//!
//! The Home Connect cloud writes UTF-8 text where a blank line ends each
//! message. One TCP chunk can carry several messages, or only a fraction of
//! one, so undelivered bytes stay buffered until a terminator arrives.

/// One parsed SSE message. Keep-alive comments arrive with no event name.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SseMessage {
    /// The `event:` field, e.g. `STATUS`, `EVENT`, `CONNECTED`, `KEEP-ALIVE`.
    pub event: Option<String>,
    /// The joined `data:` payload. Empty for keep-alive heartbeats.
    pub data: Option<String>,
    /// The `id:` field; Home Connect fills it with the appliance haId.
    pub id: Option<String>,
}

/// Feeds byte chunks in, returns complete messages.
#[derive(Debug, Default)]
pub struct SseParser {
    /// Undelivered bytes. A chunk can end anywhere, even inside a line
    /// terminator, so a partial tail is the normal case, not an error.
    buf: Vec<u8>,
}

impl SseParser {
    /// Creates an empty parser.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a chunk and returns every message that is now complete.
    /// Incomplete input stays buffered until the next chunk completes it.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseMessage> {
        self.buf.extend_from_slice(chunk);
        let mut messages = Vec::new();
        // One chunk can finish several messages; keep dispatching while a
        // full terminator sits in the buffer.
        while let Some(end) = find_terminator(&self.buf) {
            // `split_off` leaves the finished message in the buffer and
            // hands the unconsumed tail back, so the message can be parsed
            // in place and the tail becomes the next buffer.
            let tail = self.buf.split_off(end);
            if let Some(message) = parse_message(&self.buf) {
                messages.push(message);
            }
            self.buf = tail;
        }
        messages
    }
}

/// Finds the end of the first complete message, terminator included.
/// Streams mix `\n`, `\r\n` and bare `\r` line endings, so every blank-line
/// shape must count as a message end: `\n\n`, `\r\n\r\n` and `\r\r`.
fn find_terminator(buf: &[u8]) -> Option<usize> {
    for (idx, &byte) in buf.iter().enumerate() {
        match byte {
            b'\n' => {
                // The empty line after this LF ends the message.
                let next = buf.get(idx + 1)?;
                if *next == b'\n' {
                    return Some(idx + 2);
                }
                if *next == b'\r' && buf.get(idx + 2).is_some_and(|b| *b == b'\n') {
                    return Some(idx + 3);
                }
            }
            // Bare CR pairs end a message on their own; a single CR is only
            // half of a terminator and must wait for more bytes.
            b'\r' if buf.get(idx + 1) == Some(&b'\r') => return Some(idx + 2),
            _ => {}
        }
    }
    None
}

/// Turns one message's bytes, terminator included, into a message.
/// Blank noise — a message with no fields — carries nothing, so it yields
/// `None` instead of an empty heartbeat.
fn parse_message(raw: &[u8]) -> Option<SseMessage> {
    // The cloud sends UTF-8; lossy decoding keeps a stray byte from ever
    // breaking the follow loop.
    let text = String::from_utf8_lossy(raw);
    let mut event: Option<String> = None;
    let mut data: Vec<&str> = Vec::new();
    let mut id: Option<String> = None;
    // Lines end with `\n`, `\r\n` or a bare `\r`, and one message can mix
    // the three styles, so both characters split.
    for line in text.split(['\r', '\n']) {
        let Some((field, value)) = line.split_once(':') else {
            // Without a colon this is not a field; the framing drops it.
            continue;
        };
        // Exactly one space may sit between the colon and the value.
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => event = Some(value.to_owned()),
            "data" => data.push(value),
            "id" => id = Some(value.to_owned()),
            // Unknown fields (`retry:` and friends) and ':'-prefixed
            // comment lines are noise.
            _ => {}
        }
    }
    if event.is_none() && data.is_empty() && id.is_none() {
        return None;
    }
    // The first data line sets the payload, each later one appends after a
    // newline; this matches the reference client's joining rules.
    let data = if data.is_empty() {
        None
    } else {
        Some(data.join("\n"))
    };
    Some(SseMessage { event, data, id })
}

#[cfg(test)]
mod tests {
    use super::{SseMessage, SseParser};

    const STATUS_JSON: &str = r#"{"haId":"SIEMENS-WM14T6H9NL-AB1234567890","items":[{"handling":"none","key":"BSH.Common.Status.LocalControlActive","level":"hint","timestamp":1676897835.000,"uri":"/api/...","value":true},{"handling":"none","key":"BSH.Common.Status.RemoteControlActive","level":"hint","timestamp":1676897835.000,"uri":"/api/...","value":false}]}"#;
    const EVENT_JSON: &str = r#"{"items":[{"timestamp":1642001123.000,"handling":"none","key":"BSH.Common.Event.ProgramFinished","value":"BSH.Common.EnumType.EventPresentState.Present","level":"hint"}],"haId":"SIEMENS-WM14T6H9NL-AB1234567890"}"#;
    const NOTIFY_JSON: &str = r#"{"haId":"SIEMENS-WM14T6H9NL-AB1234567890","items":[{"handling":"none","key":"BSH.Common.Root.SelectedProgram","level":"hint","timestamp":1676897836.000,"uri":"/api/...","value":"LaundryCare.Washer.Program.DelicatesSilk"}]}"#;
    const CONNECTED_JSON: &str = r#"{"haId":"SIEMENS-WT8HXM90NL-AB1234567890","handling":"none","key":"BSH.Common.Appliance.Connected","level":"hint","timestamp":1676897865.000,"value":true}"#;
    const DISCONNECTED_JSON: &str = r#"{"haId":"SIEMENS-WT8HXM90NL-AB1234567890","handling":"none","key":"BSH.Common.Appliance.Disconnected","level":"hint","timestamp":1676897866.000,"value":true}"#;

    /// Builds the expected message for assertions.
    fn msg(event: Option<&str>, data: Option<&str>, id: Option<&str>) -> SseMessage {
        SseMessage {
            event: event.map(str::to_owned),
            data: data.map(str::to_owned),
            id: id.map(str::to_owned),
        }
    }

    #[test]
    fn a_message_split_across_three_chunks_is_reassembled() {
        let mut parser = SseParser::new();
        // The split points land inside a line and inside the terminator.
        assert_eq!(parser.push(b"event:STATUS\nda"), []);
        assert_eq!(parser.push(b"ta:{\"items\":[]}"), []);
        let messages = parser.push(b"\n\n");
        assert_eq!(
            messages,
            vec![msg(Some("STATUS"), Some("{\"items\":[]}"), None)]
        );
    }

    #[test]
    fn two_complete_messages_in_one_chunk() {
        let mut parser = SseParser::new();
        let messages =
            parser.push(b"event:STATUS\ndata:{\"a\":1}\n\nevent:CONNECTED\ndata:{\"b\":2}\n\n");
        assert_eq!(
            messages,
            vec![
                msg(Some("STATUS"), Some("{\"a\":1}"), None),
                msg(Some("CONNECTED"), Some("{\"b\":2}"), None),
            ]
        );
    }

    #[test]
    fn crlf_line_endings_are_accepted() {
        let mut parser = SseParser::new();
        let messages = parser.push(b"event:STATUS\r\ndata:{\"a\":1}\r\n\r\n");
        assert_eq!(messages, vec![msg(Some("STATUS"), Some("{\"a\":1}"), None)]);
    }

    #[test]
    fn a_crlf_terminator_split_across_chunks_survives() {
        // The CR of the last line and the blank line arrive separately.
        let mut parser = SseParser::new();
        assert_eq!(parser.push(b"event:STATUS\ndata:{\"a\":1}\r"), []);
        let messages = parser.push(b"\n\r\n");
        assert_eq!(messages, vec![msg(Some("STATUS"), Some("{\"a\":1}"), None)]);
    }

    #[test]
    fn a_lone_cr_pair_ends_a_message() {
        let mut parser = SseParser::new();
        // Some cloud frontends emit bare CR line endings.
        let messages = parser.push(b"event:STATUS\rdata:{\"a\":1}\r\r");
        assert_eq!(messages, vec![msg(Some("STATUS"), Some("{\"a\":1}"), None)]);
    }

    #[test]
    fn every_terminator_style_ends_a_message() {
        let mut parser = SseParser::new();
        let wire = "event:A\ndata:1\n\nevent:B\ndata:2\r\n\r\nevent:C\ndata:3\r\r";
        let messages = parser.push(wire.as_bytes());
        assert_eq!(
            messages,
            vec![
                msg(Some("A"), Some("1"), None),
                msg(Some("B"), Some("2"), None),
                msg(Some("C"), Some("3"), None),
            ]
        );
    }

    #[test]
    fn mixed_line_endings_inside_one_message() {
        let mut parser = SseParser::new();
        let messages = parser.push(b"event:STATUS\r\ndata:{\"a\":1}\ndata:2\r\n\n");
        assert_eq!(
            messages,
            vec![msg(Some("STATUS"), Some("{\"a\":1}\n2"), None)]
        );
    }

    #[test]
    fn comment_lines_are_ignored() {
        let mut parser = SseParser::new();
        // A comment-only block dispatches nothing; comments between fields
        // leave them untouched.
        let messages =
            parser.push(b": stream open\n\n: comment\nevent:STATUS\n: ping\ndata:{\"a\":1}\n\n");
        assert_eq!(messages, vec![msg(Some("STATUS"), Some("{\"a\":1}"), None)]);
    }

    #[test]
    fn two_data_lines_join_with_a_newline() {
        let mut parser = SseParser::new();
        let messages = parser.push(b"event:EVENT\ndata:{\"a\":\ndata:1}\n\n");
        assert_eq!(
            messages,
            vec![msg(Some("EVENT"), Some("{\"a\":\n1}"), None)]
        );
    }

    #[test]
    fn an_incomplete_tail_stays_buffered() {
        let mut parser = SseParser::new();
        // A complete message followed by the start of the next one.
        let messages = parser.push(b"event:STATUS\ndata:1\n\nevent:EVENT\ndata:2");
        assert_eq!(messages, vec![msg(Some("STATUS"), Some("1"), None)]);
        let rest = parser.push(b"\n\n");
        assert_eq!(rest, vec![msg(Some("EVENT"), Some("2"), None)]);
    }

    #[test]
    fn a_retry_line_is_ignored() {
        let mut parser = SseParser::new();
        // Home Connect never sends `retry:`, but the SSE framing allows it.
        let messages = parser.push(b"retry:5000\nevent:STATUS\ndata:1\n\n");
        assert_eq!(messages, vec![msg(Some("STATUS"), Some("1"), None)]);
    }

    #[test]
    fn one_space_after_the_colon_is_stripped() {
        let mut parser = SseParser::new();
        let messages = parser.push(b"event: STATUS\ndata: 1\n\n");
        assert_eq!(messages, vec![msg(Some("STATUS"), Some("1"), None)]);
    }

    #[test]
    fn the_real_samples_parse() {
        let mut parser = SseParser::new();
        let wire = format!(
            "event:STATUS\nid:SIEMENS-WM14T6H9NL-AB1234567890\ndata: {STATUS_JSON}\n\n\
             event:EVENT\ndata:{EVENT_JSON}\n\n\
             event:NOTIFY\ndata:{NOTIFY_JSON}\n\n\
             event:CONNECTED\ndata:{CONNECTED_JSON}\n\n\
             event:DISCONNECTED\ndata:{DISCONNECTED_JSON}\n\n\
             event:KEEP-ALIVE\ndata:\n\n"
        );
        let messages = parser.push(wire.as_bytes());
        assert_eq!(
            messages,
            vec![
                msg(
                    Some("STATUS"),
                    Some(STATUS_JSON),
                    Some("SIEMENS-WM14T6H9NL-AB1234567890")
                ),
                msg(Some("EVENT"), Some(EVENT_JSON), None),
                msg(Some("NOTIFY"), Some(NOTIFY_JSON), None),
                msg(Some("CONNECTED"), Some(CONNECTED_JSON), None),
                msg(Some("DISCONNECTED"), Some(DISCONNECTED_JSON), None),
                msg(Some("KEEP-ALIVE"), Some(""), None),
            ]
        );
    }
}
