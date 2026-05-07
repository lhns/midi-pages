//! Lightweight tagging of inbound MIDI bytes into the variants the proxy cares about.
//!
//! We deliberately do not implement a full MIDI parser. The proxy only needs to
//! distinguish: NoteOn, NoteOff, ControlChange, SysEx, "everything else". Running
//! status is unsupported (USB MIDI never uses it).

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Msg<'a> {
    NoteOn {
        channel: u8,
        note: u8,
        velocity: u8,
    },
    NoteOff {
        channel: u8,
        note: u8,
        velocity: u8,
    },
    Cc {
        channel: u8,
        controller: u8,
        value: u8,
    },
    SysEx(&'a [u8]),
    Other(&'a [u8]),
}

pub fn classify(bytes: &[u8]) -> Msg<'_> {
    if bytes.is_empty() {
        return Msg::Other(bytes);
    }
    let status = bytes[0];
    match status & 0xF0 {
        0x80 if bytes.len() >= 3 => Msg::NoteOff {
            channel: status & 0x0F,
            note: bytes[1],
            velocity: bytes[2],
        },
        0x90 if bytes.len() >= 3 => {
            // Convention: NoteOn with velocity 0 is a NoteOff.
            if bytes[2] == 0 {
                Msg::NoteOff {
                    channel: status & 0x0F,
                    note: bytes[1],
                    velocity: 0,
                }
            } else {
                Msg::NoteOn {
                    channel: status & 0x0F,
                    note: bytes[1],
                    velocity: bytes[2],
                }
            }
        }
        0xB0 if bytes.len() >= 3 => Msg::Cc {
            channel: status & 0x0F,
            controller: bytes[1],
            value: bytes[2],
        },
        _ if status == 0xF0 => Msg::SysEx(bytes),
        _ => Msg::Other(bytes),
    }
}

#[inline]
pub fn note_on(channel: u8, note: u8, velocity: u8) -> [u8; 3] {
    [0x90 | (channel & 0x0F), note & 0x7F, velocity & 0x7F]
}

#[inline]
pub fn note_off(channel: u8, note: u8, velocity: u8) -> [u8; 3] {
    [0x80 | (channel & 0x0F), note & 0x7F, velocity & 0x7F]
}

#[inline]
pub fn cc(channel: u8, controller: u8, value: u8) -> [u8; 3] {
    [0xB0 | (channel & 0x0F), controller & 0x7F, value & 0x7F]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_note_on() {
        assert_eq!(
            classify(&[0x91, 60, 100]),
            Msg::NoteOn {
                channel: 1,
                note: 60,
                velocity: 100
            }
        );
    }

    #[test]
    fn note_on_velocity_zero_is_note_off() {
        assert_eq!(
            classify(&[0x90, 60, 0]),
            Msg::NoteOff {
                channel: 0,
                note: 60,
                velocity: 0
            }
        );
    }

    #[test]
    fn classifies_cc() {
        assert_eq!(
            classify(&[0xB0, 91, 1]),
            Msg::Cc {
                channel: 0,
                controller: 91,
                value: 1
            }
        );
    }

    #[test]
    fn classifies_sysex() {
        let bytes = [0xF0, 0x00, 0x20, 0x29, 0x02, 0x0D, 0x0E, 0x01, 0xF7];
        match classify(&bytes) {
            Msg::SysEx(s) => assert_eq!(s, &bytes),
            other => panic!("not sysex: {other:?}"),
        }
    }

    #[test]
    fn empty_is_other() {
        assert_eq!(classify(&[]), Msg::Other(&[]));
    }

    #[test]
    fn truncated_note_on_is_other() {
        assert_eq!(classify(&[0x90, 60]), Msg::Other(&[0x90, 60][..]));
    }
}
