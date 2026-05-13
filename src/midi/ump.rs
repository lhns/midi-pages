//! Minimal MIDI 1.0 byte stream ↔ UMP (Universal MIDI Packet) transcoder for
//! the Windows MIDI Services Virtual Device transport. Handles MT2 (Channel
//! Voice) and MT3 (Data / SysEx) packets. Higher-level MT4 (MIDI 2.0 CVM) and
//! MT5 (extended data) are not used because we declare `SupportsMidi20Protocol = false`
//! on our virtual endpoints, so peers send MIDI-1.0-byte-format UMP only.

/// Encode a MIDI 1.0 byte stream as a sequence of UMP 32-bit words. The input
/// must be ONE complete MIDI message (Note On / Note Off / CC / SysEx, etc.),
/// not a stream with interleaved messages.
pub fn encode(bytes: &[u8], group: u8) -> Vec<u32> {
    if bytes.is_empty() {
        return Vec::new();
    }
    let status = bytes[0];
    if status == 0xF0 {
        return encode_sysex(bytes, group);
    }
    if (0x80..0xF0).contains(&status) {
        // Channel voice: 3 bytes (Note On/Off/CC/PolyKey/PitchBend) or 2 bytes (ProgramChange/ChannelPressure).
        let d1 = bytes.get(1).copied().unwrap_or(0);
        let d2 = bytes.get(2).copied().unwrap_or(0);
        let word = mt2_word(group, status, d1, d2);
        return vec![word];
    }
    // System common / real-time: 1-3 bytes, MT1. We don't use these but encode
    // them so the proxy can transparently forward any message a DAW sends.
    let d1 = bytes.get(1).copied().unwrap_or(0);
    let d2 = bytes.get(2).copied().unwrap_or(0);
    vec![
        (0x1_u32 << 28)
            | ((group as u32 & 0xF) << 24)
            | ((status as u32) << 16)
            | ((d1 as u32) << 8)
            | (d2 as u32),
    ]
}

fn mt2_word(group: u8, status: u8, data1: u8, data2: u8) -> u32 {
    (0x2_u32 << 28)
        | ((group as u32 & 0xF) << 24)
        | ((status as u32) << 16)
        | ((data1 as u32) << 8)
        | (data2 as u32)
}

fn encode_sysex(bytes: &[u8], group: u8) -> Vec<u32> {
    // Strip F0 / F7 framing for UMP payload — UMP MT3 has its own framing.
    let payload_start = 1; // skip F0
    let payload_end = if bytes.last() == Some(&0xF7) {
        bytes.len() - 1
    } else {
        bytes.len()
    };
    let payload = &bytes[payload_start..payload_end];

    let mut words = Vec::with_capacity(payload.len() / 6 * 2 + 4);
    let chunks: Vec<&[u8]> = payload.chunks(6).collect();
    let total = chunks.len();
    if total == 0 {
        // Zero-byte SysEx (just F0 F7). Send a single "complete" packet with 0 bytes.
        words.extend_from_slice(&mt3_packet(group, 0x0, &[]));
        return words;
    }
    for (i, chunk) in chunks.iter().enumerate() {
        let status = match (total, i) {
            (1, 0) => 0x0,                         // single complete
            (_, 0) => 0x1,                         // start
            (_, last) if last == total - 1 => 0x3, // end
            _ => 0x2,                              // continue
        };
        words.extend_from_slice(&mt3_packet(group, status, chunk));
    }
    words
}

fn mt3_packet(group: u8, status_nibble: u8, payload: &[u8]) -> [u32; 2] {
    debug_assert!(payload.len() <= 6);
    let n = payload.len() as u8;
    let mut word1 = (0x3_u32 << 28)
        | ((group as u32 & 0xF) << 24)
        | ((status_nibble as u32 & 0xF) << 20)
        | ((n as u32 & 0xF) << 16);
    let mut word2 = 0u32;
    if !payload.is_empty() {
        word1 |= (payload[0] as u32) << 8;
    }
    if payload.len() >= 2 {
        word1 |= payload[1] as u32;
    }
    if payload.len() >= 3 {
        word2 |= (payload[2] as u32) << 24;
    }
    if payload.len() >= 4 {
        word2 |= (payload[3] as u32) << 16;
    }
    if payload.len() >= 5 {
        word2 |= (payload[4] as u32) << 8;
    }
    if payload.len() >= 6 {
        word2 |= payload[5] as u32;
    }
    [word1, word2]
}

/// State machine for decoding UMP word stream back into MIDI 1.0 byte messages.
/// Each call to `feed` may emit zero or more complete byte-format MIDI messages.
#[derive(Debug, Default)]
pub struct Decoder {
    sysex_buf: Vec<u8>,
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed UMP words; returns complete MIDI 1.0 byte messages assembled so far.
    pub fn feed(&mut self, words: &[u32]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < words.len() {
            let w = words[i];
            let mt = (w >> 28) & 0xF;
            match mt {
                0x0 => {
                    // MT0 Utility: NOOP / JR Clock / JR Timestamp. No byte-format
                    // equivalent — silently consume the word.
                    i += 1;
                }
                0x1 | 0x2 => {
                    // MT1 System Real Time / System Common, MT2 MIDI 1.0 Channel
                    // Voice. Both 1 word, byte-format MIDI 1.0 message in
                    // bits 16-23 (status) + 8-15 (d1) + 0-7 (d2).
                    let s = ((w >> 16) & 0xFF) as u8;
                    let d1 = ((w >> 8) & 0xFF) as u8;
                    let d2 = (w & 0xFF) as u8;
                    let len = byte_format_len(s);
                    let mut bytes = Vec::with_capacity(len);
                    bytes.push(s);
                    if len >= 2 {
                        bytes.push(d1);
                    }
                    if len >= 3 {
                        bytes.push(d2);
                    }
                    out.push(bytes);
                    i += 1;
                }
                0x3 => {
                    // MT3 Data (SysEx 7-bit): 2 words per packet.
                    if i + 1 >= words.len() {
                        break; // truncated; wait for more
                    }
                    let w0 = words[i];
                    let w1 = words[i + 1];
                    let status = ((w0 >> 20) & 0xF) as u8;
                    let n = ((w0 >> 16) & 0xF) as usize;
                    let bytes = mt3_extract(w0, w1, n);
                    match status {
                        0x0 => {
                            // single complete
                            let mut full = Vec::with_capacity(n + 2);
                            full.push(0xF0);
                            full.extend_from_slice(&bytes);
                            full.push(0xF7);
                            out.push(full);
                        }
                        0x1 => {
                            // start
                            self.sysex_buf.clear();
                            self.sysex_buf.push(0xF0);
                            self.sysex_buf.extend_from_slice(&bytes);
                        }
                        0x2 => {
                            // continue
                            self.sysex_buf.extend_from_slice(&bytes);
                        }
                        0x3 => {
                            // end
                            self.sysex_buf.extend_from_slice(&bytes);
                            self.sysex_buf.push(0xF7);
                            out.push(std::mem::take(&mut self.sysex_buf));
                        }
                        _ => {}
                    }
                    i += 2;
                }
                0x4 => {
                    // MT4 MIDI 2.0 channel voice: 2 words. We don't expect this on
                    // MIDI-1.0-protocol endpoints; skip silently.
                    i += 2;
                }
                0x5..=0xF => {
                    // 128-bit packets — MT5..MT8 reserved, MT5 mixed data set,
                    // MT9..MTC reserved, MTD Flex Data, MTE reserved, MTF UMP
                    // Stream (endpoint/function-block discovery, stream config
                    // negotiation, etc.). All four words. Skip them as a whole
                    // — otherwise the trailing words are mis-read from the top
                    // and produce phantom byte-format messages.
                    i += 4;
                }
                _ => {
                    // mt is a 4-bit field; the arms above cover 0x0..=0xF.
                    // Defensive fallback — advance one word.
                    i += 1;
                }
            }
        }
        out
    }
}

fn mt3_extract(w0: u32, w1: u32, n: usize) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(n);
    if n >= 1 {
        bytes.push(((w0 >> 8) & 0xFF) as u8);
    }
    if n >= 2 {
        bytes.push((w0 & 0xFF) as u8);
    }
    if n >= 3 {
        bytes.push(((w1 >> 24) & 0xFF) as u8);
    }
    if n >= 4 {
        bytes.push(((w1 >> 16) & 0xFF) as u8);
    }
    if n >= 5 {
        bytes.push(((w1 >> 8) & 0xFF) as u8);
    }
    if n >= 6 {
        bytes.push((w1 & 0xFF) as u8);
    }
    bytes
}

fn byte_format_len(status: u8) -> usize {
    match status & 0xF0 {
        0x80 | 0x90 | 0xA0 | 0xB0 | 0xE0 => 3,
        0xC0 | 0xD0 => 2,
        0xF0 => match status {
            0xF1 | 0xF3 => 2,
            0xF2 => 3,
            _ => 1,
        },
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_on_round_trip() {
        let bytes = vec![0x90u8, 60, 100];
        let words = encode(&bytes, 0);
        assert_eq!(words, vec![0x20903C64]);
        let decoded = Decoder::new().feed(&words);
        assert_eq!(decoded, vec![bytes]);
    }

    #[test]
    fn cc_round_trip() {
        let bytes = vec![0xB0u8, 91, 1];
        let words = encode(&bytes, 0);
        assert_eq!(words.len(), 1);
        let decoded = Decoder::new().feed(&words);
        assert_eq!(decoded, vec![bytes]);
    }

    #[test]
    fn sysex_small_round_trip() {
        // F0 00 20 29 02 0D 0E 01 F7  — programmer mode select (8 payload bytes)
        let bytes = vec![0xF0, 0x00, 0x20, 0x29, 0x02, 0x0D, 0x0E, 0x01, 0xF7];
        let words = encode(&bytes, 0);
        // 8 payload bytes => 2 MT3 packets (start with 6 bytes, end with 2 bytes) => 4 words.
        assert_eq!(words.len(), 4);
        let decoded = Decoder::new().feed(&words);
        assert_eq!(decoded, vec![bytes]);
    }

    #[test]
    fn sysex_large_round_trip() {
        // Simulate a 64-LED bulk lighting SysEx (~328 bytes)
        let mut bytes = vec![0xF0, 0x00, 0x20, 0x29, 0x02, 0x0D, 0x03];
        for i in 0u8..64 {
            bytes.extend_from_slice(&[0x03, 11 + i, i, 0, 0]); // RGB spec, led_index, R, G, B
        }
        bytes.push(0xF7);
        let words = encode(&bytes, 0);
        let decoded = Decoder::new().feed(&words);
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0], bytes);
    }

    #[test]
    fn sysex_exact_6_bytes_uses_single_complete() {
        let bytes = vec![0xF0, 1, 2, 3, 4, 5, 6, 0xF7]; // 6 payload bytes
        let words = encode(&bytes, 0);
        assert_eq!(words.len(), 2); // one MT3 single packet = 2 words
        // status nibble in word[0] bits 20-23 should be 0x0 (single complete)
        assert_eq!((words[0] >> 20) & 0xF, 0x0);
        let decoded = Decoder::new().feed(&words);
        assert_eq!(decoded, vec![bytes]);
    }

    // ---- MT skip table (regression for the phantom-LED bug) ---------------

    /// MT0 (Utility) — NOOP / JR Clock / JR Timestamp — has no byte-format
    /// equivalent. Decoding it must emit nothing, no matter what's in the
    /// data bits. Previously we treated MT0 like MT1 and emitted a synthetic
    /// 1-byte message; harmless to a Launchpad but conceptually wrong.
    #[test]
    fn mt0_utility_does_not_emit() {
        // All MT0 words (top nibble = 0). Status sub-field at bits 20-23 ranges
        // over NOOP (0), JR Clock (1), JR Timestamp (2); the rest of the bits
        // are arbitrary timestamp/clock payload. The decoder must consume each
        // word silently — emitting nothing.
        let words = [
            0x00000000_u32,       // NOOP
            0x00112233,           // NOOP with junk in data field
            0x00100000 | 0x1234,  // JR Clock
            0x00200000 | 0xFFFFF, // JR Timestamp (max value)
        ];
        assert!(Decoder::new().feed(&words).is_empty());
    }

    /// MT 0xF (UMP Stream) is a 128-bit (4-word) packet. WMS commonly emits
    /// Stream messages for endpoint discovery / function-block info / stream
    /// config — interleaved with channel-voice. The decoder must consume all
    /// 4 words atomically; if it only skips 1, the next 3 words get re-read
    /// from the top and their bit pattern may look like a channel-voice
    /// NoteOn — the exact phantom-LED mechanism reported by the user.
    #[test]
    fn mt_stream_consumes_four_words_not_one() {
        // 4 Stream words crafted so each *individual* word's top nibble looks
        // like a different MT (0x2 = channel voice, 0x9 = reserved 128-bit).
        // Followed by a real MT2 NoteOn that must be the ONLY emitted message.
        let stream = [
            0xF0001234_u32, // word 0: MT 0xF, Format 00 (single), status, data
            0x20904160,     // word 1 — pre-fix would have decoded this as a
            // channel-voice NoteOn note 0x41 vel 0x60.
            0x29904271, // word 2 — top nibble 0x2, status bits 0x90 etc.
            0x00DEADBE, // word 3 — looks like MT0 if mis-read
        ];
        let note_on = encode(&[0x90, 60, 100], 0);
        let mut feed_buf = Vec::from(&stream[..]);
        feed_buf.extend_from_slice(&note_on);

        let out = Decoder::new().feed(&feed_buf);
        assert_eq!(out, vec![vec![0x90, 60, 100]]);
    }

    /// Same coverage for MT 0xD (Flex Data). Different MT, same 4-word size,
    /// same misread risk.
    #[test]
    fn mt_flex_data_consumes_four_words_not_one() {
        let flex = [0xD0001234_u32, 0x20904160, 0x29904271, 0x00112233];
        let note_on = encode(&[0x90, 60, 100], 0);
        let mut buf = Vec::from(&flex[..]);
        buf.extend_from_slice(&note_on);
        assert_eq!(Decoder::new().feed(&buf), vec![vec![0x90, 60, 100]]);
    }

    /// MT5 (mixed data set, 4 words) was already covered in the implementation
    /// but had no test — lock it down.
    #[test]
    fn mt5_consumes_four_words_not_one() {
        let mds = [0x50001234_u32, 0x20904160, 0x29904271, 0x00112233];
        let note_on = encode(&[0x90, 60, 100], 0);
        let mut buf = Vec::from(&mds[..]);
        buf.extend_from_slice(&note_on);
        assert_eq!(Decoder::new().feed(&buf), vec![vec![0x90, 60, 100]]);
    }
}
