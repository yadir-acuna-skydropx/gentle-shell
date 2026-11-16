//! Transport framing between the gentle-session client and server (S34).
//!
//! The content bytes pass through unchanged (no terminal emulation), so keys,
//! mouse, bracketed paste and OSC 52 work exactly as if the PTY were local.
//! Framing only separates terminal data from the two tiny control messages
//! (resize, and the server's detach/exit notices).
//!
//! Client -> server:
//!   DATA   = [0x01][u32 len][payload]   stdin bytes for the PTY
//!   RESIZE = [0x02][u16 rows][u16 cols] window size (sent on attach + SIGWINCH)
//!
//! Server -> client:
//!   DATA   = [0x01][u32 len][payload]   PTY output bytes for stdout
//!   EXIT   = [0x02][i32 code]           the child exited; client exits `code`
//!   DETACH = [0x03]                     taken over by a new client

use std::io::{self, Read, Write};

pub const C2S_DATA: u8 = 0x01;
pub const C2S_RESIZE: u8 = 0x02;

pub const S2C_DATA: u8 = 0x01;
pub const S2C_EXIT: u8 = 0x02;
pub const S2C_DETACH: u8 = 0x03;

/// A message from the client to the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientMsg {
    Data(Vec<u8>),
    Resize { rows: u16, cols: u16 },
}

/// A message from the server to the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerMsg {
    Data(Vec<u8>),
    Exit(i32),
    Detach,
}

impl ClientMsg {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            ClientMsg::Data(bytes) => {
                let mut out = Vec::with_capacity(5 + bytes.len());
                out.push(C2S_DATA);
                out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
                out.extend_from_slice(bytes);
                out
            }
            ClientMsg::Resize { rows, cols } => {
                let mut out = Vec::with_capacity(5);
                out.push(C2S_RESIZE);
                out.extend_from_slice(&rows.to_be_bytes());
                out.extend_from_slice(&cols.to_be_bytes());
                out
            }
        }
    }

    /// Reads one client message, or `None` at a clean end of stream.
    pub fn read_from(r: &mut impl Read) -> io::Result<Option<ClientMsg>> {
        let mut tag = [0u8; 1];
        match r.read_exact(&mut tag) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }
        match tag[0] {
            C2S_DATA => {
                let mut len = [0u8; 4];
                r.read_exact(&mut len)?;
                let n = u32::from_be_bytes(len) as usize;
                let mut buf = vec![0u8; n];
                r.read_exact(&mut buf)?;
                Ok(Some(ClientMsg::Data(buf)))
            }
            C2S_RESIZE => {
                let mut dims = [0u8; 4];
                r.read_exact(&mut dims)?;
                let rows = u16::from_be_bytes([dims[0], dims[1]]);
                let cols = u16::from_be_bytes([dims[2], dims[3]]);
                Ok(Some(ClientMsg::Resize { rows, cols }))
            }
            other => Err(io::Error::new(io::ErrorKind::InvalidData, format!("bad client tag {other}"))),
        }
    }

    pub fn write_to(&self, w: &mut impl Write) -> io::Result<()> {
        w.write_all(&self.encode())?;
        w.flush()
    }
}

impl ServerMsg {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            ServerMsg::Data(bytes) => {
                let mut out = Vec::with_capacity(5 + bytes.len());
                out.push(S2C_DATA);
                out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
                out.extend_from_slice(bytes);
                out
            }
            ServerMsg::Exit(code) => {
                let mut out = Vec::with_capacity(5);
                out.push(S2C_EXIT);
                out.extend_from_slice(&code.to_be_bytes());
                out
            }
            ServerMsg::Detach => vec![S2C_DETACH],
        }
    }

    /// Reads one server message, or `None` at a clean end of stream.
    pub fn read_from(r: &mut impl Read) -> io::Result<Option<ServerMsg>> {
        let mut tag = [0u8; 1];
        match r.read_exact(&mut tag) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }
        match tag[0] {
            S2C_DATA => {
                let mut len = [0u8; 4];
                r.read_exact(&mut len)?;
                let n = u32::from_be_bytes(len) as usize;
                let mut buf = vec![0u8; n];
                r.read_exact(&mut buf)?;
                Ok(Some(ServerMsg::Data(buf)))
            }
            S2C_EXIT => {
                let mut code = [0u8; 4];
                r.read_exact(&mut code)?;
                Ok(Some(ServerMsg::Exit(i32::from_be_bytes(code))))
            }
            S2C_DETACH => Ok(Some(ServerMsg::Detach)),
            other => Err(io::Error::new(io::ErrorKind::InvalidData, format!("bad server tag {other}"))),
        }
    }

    pub fn write_to(&self, w: &mut impl Write) -> io::Result<()> {
        w.write_all(&self.encode())?;
        w.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_data_round_trips_bytes_unchanged() {
        // Raw passthrough: CSI-u / OSC 52 / mouse bytes must survive verbatim.
        let payload = b"\x1b[13;2u\x1b]52;c;Zm9v\x07\x1b[<0;1;1M".to_vec();
        let msg = ClientMsg::Data(payload.clone());
        let mut cursor = io::Cursor::new(msg.encode());
        assert_eq!(ClientMsg::read_from(&mut cursor).unwrap(), Some(ClientMsg::Data(payload)));
    }

    #[test]
    fn client_resize_round_trips() {
        let msg = ClientMsg::Resize { rows: 50, cols: 200 };
        let mut cursor = io::Cursor::new(msg.encode());
        assert_eq!(ClientMsg::read_from(&mut cursor).unwrap(), Some(msg));
    }

    #[test]
    fn server_messages_round_trip() {
        for msg in [ServerMsg::Data(vec![1, 2, 3]), ServerMsg::Exit(-7), ServerMsg::Detach] {
            let mut cursor = io::Cursor::new(msg.encode());
            assert_eq!(ServerMsg::read_from(&mut cursor).unwrap(), Some(msg));
        }
    }

    #[test]
    fn empty_stream_reads_as_none() {
        let mut empty = io::Cursor::new(Vec::new());
        assert_eq!(ClientMsg::read_from(&mut empty).unwrap(), None);
        let mut empty = io::Cursor::new(Vec::new());
        assert_eq!(ServerMsg::read_from(&mut empty).unwrap(), None);
    }

    #[test]
    fn multiple_frames_decode_in_order() {
        let mut bytes = Vec::new();
        bytes.extend(ClientMsg::Resize { rows: 24, cols: 80 }.encode());
        bytes.extend(ClientMsg::Data(b"hi".to_vec()).encode());
        let mut cursor = io::Cursor::new(bytes);
        assert_eq!(ClientMsg::read_from(&mut cursor).unwrap(), Some(ClientMsg::Resize { rows: 24, cols: 80 }));
        assert_eq!(ClientMsg::read_from(&mut cursor).unwrap(), Some(ClientMsg::Data(b"hi".to_vec())));
        assert_eq!(ClientMsg::read_from(&mut cursor).unwrap(), None);
    }
}
