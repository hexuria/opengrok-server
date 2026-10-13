//! The client's half of a websocket, as much of it as carrying a VNC session needs: the box's
//! viewer (websockify) speaks RFB inside websocket frames, and a VNC app on the person's machine
//! speaks raw RFB down the SSH forward. RFC 6455: a client masks what it sends, a server does not;
//! a ping is answered with a pong; a close ends the session.

/// The largest frame taken from the box: a VNC update is far smaller, and one past this is not a
/// screen talking.
const MAX_FRAME: usize = 16 * 1024 * 1024;

const BINARY: u8 = 0x2;
const CLOSE: u8 = 0x8;
const PING: u8 = 0x9;
const PONG: u8 = 0xA;

/// One frame for the box: final, masked with `mask`, `opcode` (binary for data).
pub fn frame(opcode: u8, payload: &[u8], mask: [u8; 4]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 14);
    out.push(0x80 | opcode);
    match payload.len() {
        len @ 0..=125 => out.push(0x80 | len as u8),
        len @ 126..=0xFFFF => {
            out.push(0x80 | 126);
            out.extend_from_slice(&(len as u16).to_be_bytes());
        }
        len => {
            out.push(0x80 | 127);
            out.extend_from_slice(&(len as u64).to_be_bytes());
        }
    }
    out.extend_from_slice(&mask);
    out.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    out
}

/// A data frame for the box.
pub fn binary(payload: &[u8], mask: [u8; 4]) -> Vec<u8> {
    frame(BINARY, payload, mask)
}

/// What the box sent, frame by frame.
#[derive(Debug, PartialEq, Eq)]
pub enum Heard {
    /// Bytes of the VNC session (a binary, text or continuation frame's payload).
    Data(Vec<u8>),
    /// A ping, to be answered with a pong carrying this.
    Ping(Vec<u8>),
    /// The box closed the socket.
    Close,
    /// A pong, or anything else that carries nothing for the session.
    Nothing,
}

/// Reads frames off what arrives from the box, however the bytes are split.
#[derive(Debug, Default)]
pub struct Reader {
    pending: Vec<u8>,
}

impl Reader {
    pub fn push(&mut self, bytes: &[u8]) {
        self.pending.extend_from_slice(bytes);
    }

    /// The next whole frame, `Ok(None)` until one has arrived, or why the stream is not a
    /// websocket a screen would send.
    pub fn next_frame(&mut self) -> Result<Option<Heard>, &'static str> {
        let buf = &self.pending;
        let [first, second, ..] = buf[..] else {
            return Ok(None);
        };
        let (masked, short) = (second & 0x80 != 0, usize::from(second & 0x7F));
        let (len, mut at) = match short {
            126 if buf.len() >= 4 => (usize::from(u16::from_be_bytes([buf[2], buf[3]])), 4),
            127 if buf.len() >= 10 => {
                let mut wide = [0u8; 8];
                wide.copy_from_slice(&buf[2..10]);
                let len =
                    usize::try_from(u64::from_be_bytes(wide)).map_err(|_| "a frame too big")?;
                (len, 10)
            }
            126 | 127 => return Ok(None),
            len => (len, 2),
        };
        if len > MAX_FRAME {
            return Err("a frame too big for a screen");
        }
        let mask = if masked {
            let Some(key) = buf.get(at..at + 4) else {
                return Ok(None);
            };
            at += 4;
            Some([key[0], key[1], key[2], key[3]])
        } else {
            None
        };
        if buf.len() < at + len {
            return Ok(None);
        }
        let mut payload = buf[at..at + len].to_vec();
        if let Some(mask) = mask {
            payload
                .iter_mut()
                .enumerate()
                .for_each(|(i, b)| *b ^= mask[i % 4]);
        }
        self.pending.drain(..at + len);
        Ok(Some(match first & 0x0F {
            0x0..=0x2 => Heard::Data(payload),
            CLOSE => Heard::Close,
            PING => Heard::Ping(payload),
            _ => Heard::Nothing,
        }))
    }
}

/// A pong for the box, answering its ping.
pub fn pong(payload: &[u8], mask: [u8; 4]) -> Vec<u8> {
    frame(PONG, payload, mask)
}

/// A close for the box.
pub fn close(mask: [u8; 4]) -> Vec<u8> {
    frame(CLOSE, &[], mask)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "../tests/unit/ws.rs"]
mod tests;
