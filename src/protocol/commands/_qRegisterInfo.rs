use super::prelude::*;

#[derive(Debug)]
pub struct qRegisterInfo(pub usize);

impl<'a> ParseCommand<'a> for qRegisterInfo {
    fn from_packet(buf: PacketBuf<'a>) -> Option<Self> {
        let body = buf.into_body();
        let n: usize = std::str::from_utf8(body).ok()?.parse().ok()?;
        Some(qRegisterInfo(n))
    }
}
