//! Validation relay sessions: the dedicated outbound connections that carry our validations to
//! well-connected hubs.
//!
//! A hub pings each peer on a 60-second timer and drops one that has not answered the previous
//! ping by the next tick (rippled `PeerImp::onTimer`, "Ping Timeout"). A relay that only writes
//! therefore loses its connection about two minutes after opening it, and finds out only when the
//! next validation it writes fails. A session here reads the hub's messages and answers its pings,
//! notices at once when the hub hangs up, and the next session re-sends the last validation.

use std::time::{Duration, Instant};

use futures::StreamExt;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{broadcast, mpsc};
use tokio_util::codec::FramedRead;

use super::codec::{is_validation_frame, pong_frame_for, MessageCodec};
use crate::NodeError;

/// How long a validation stays worth re-sending on a new session: well inside the three minutes
/// after its signing time at which a peer discards it as not current and charges the sender for
/// useless data (rippled `ValidationParms::validationCurrentEarly`, `isCurrent`).
pub const RESEND_WITHIN: Duration = Duration::from_secs(60);

/// The hubs that carry our validations, beside the regular peer connections: one relay per slot,
/// on the slot's hubs in turn (the next one after each attempt that fails). On 2026-09-26:
/// - xrplcluster.com was dropped: the name leads to a web proxy with no peer port, and every
///   handshake with it failed.
/// - hub.xrpl-commons.org (on rippled's bootstrap list, `OverlayImpl::start`) answered 400 "Failed to
///   verify session" to the handshake the Ripple hubs accept, so it is not used.
/// - hubs.xrpkuwait.com (also on that list) and zaphod.alloy.ee answered 503: full. The fourth relay
///   alternates between them until one has a free slot.
///
/// Check the list with `cargo test -p xrpl-node --lib every_relay_hub_answers -- --ignored`.
pub const RELAY_HUBS: [&[&str]; 4] = [
    &["s1.ripple.com:51235"],
    &["s2.ripple.com:51235"],
    &["r.ripple.com:51235"],
    &["hubs.xrpkuwait.com:51235", "zaphod.alloy.ee:51235"],
];

/// The hub of a slot to try after `failures` failed attempts in a row.
pub fn hub_for_attempt<'a>(hubs: &[&'a str], failures: u32) -> &'a str {
    hubs[failures as usize % hubs.len()]
}

/// How long a relay waits after `failures` attempts in a row that did not open a session: 10 s,
/// doubling, at most 10 minutes. A hub that keeps refusing is then tried six times an hour instead
/// of every 25 seconds.
pub fn retry_delay(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(6);
    Duration::from_secs((10u64 << doublings).min(600))
}

/// What a relay carries from one session to the next.
#[derive(Debug, Default)]
pub struct RelayMemory {
    last_validation: Option<(Instant, Vec<u8>)>,
}

impl RelayMemory {
    /// Note a frame this relay is about to forward at `at`; only validations are remembered.
    pub fn forwarded(&mut self, frame: &[u8], at: Instant) {
        if is_validation_frame(frame) {
            self.last_validation = Some((at, frame.to_vec()));
        }
    }

    /// The validation to re-send on a session opened at `now`, if it is recent enough to count.
    pub fn resend_at(&self, now: Instant) -> Option<&[u8]> {
        match &self.last_validation {
            Some((at, frame)) if now.saturating_duration_since(*at) < RESEND_WITHIN => Some(frame),
            _ => None,
        }
    }
}

/// Why a relay session ended.
#[derive(Debug)]
pub enum SessionEnd {
    /// A write to the peer failed.
    WriteFailed(std::io::Error),
    /// The peer sent something that could not be read as a protocol frame.
    ReadFailed(NodeError),
    /// The peer closed the connection.
    PeerClosed,
    /// The validator's broadcast closed: nothing is left to relay.
    Shutdown,
}

/// How a relay session went.
#[derive(Debug)]
pub struct SessionReport {
    /// Why it ended.
    pub end: SessionEnd,
    /// Keep-alive pings it answered.
    pub pongs: u32,
}

/// Open a session on a freshly handshaken connection: send our manifest, so the peer can tie our
/// validations to our validator key, then re-send the previous session's last validation if it is
/// still recent. Returns whether a validation was re-sent.
pub async fn open_session<W>(stream: &mut W, manifest: &[u8], memory: &RelayMemory) -> std::io::Result<bool>
where
    W: AsyncWrite + Unpin,
{
    stream.write_all(manifest).await?;
    match memory.resend_at(Instant::now()) {
        Some(frame) => {
            stream.write_all(frame).await?;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// Relay on an opened session until it ends: forward every frame the validator broadcasts and
/// answer the peer's pings. `leftover` is whatever the handshake read past the HTTP headers, which
/// already belongs to the framed protocol.
pub async fn run_session<S>(
    stream: S,
    leftover: Vec<u8>,
    mut outbound: broadcast::Receiver<Vec<u8>>,
    memory: &mut RelayMemory,
) -> SessionReport
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (reader, mut writer) = tokio::io::split(stream);
    let (pong_tx, mut pong_rx) = mpsc::channel::<Vec<u8>>(8);
    let mut reader_task = tokio::spawn(async move {
        // The handshake's leftover bytes are read first, like the rest of the stream: pushed into
        // the framed buffer instead, they would wait undecoded until the socket delivered more.
        let mut framed = FramedRead::new(std::io::Cursor::new(leftover).chain(reader), MessageCodec);
        while let Some(frame) = framed.next().await {
            match frame {
                Ok(msg) => {
                    if let Some(pong) = pong_frame_for(&msg) {
                        if pong_tx.send(pong).await.is_err() {
                            return None;
                        }
                    }
                }
                Err(e) => return Some(e),
            }
        }
        None
    });

    let mut pongs = 0u32;
    let end = loop {
        tokio::select! {
            got = outbound.recv() => match got {
                Ok(frame) => {
                    memory.forwarded(&frame, Instant::now());
                    if let Err(e) = writer.write_all(&frame).await {
                        break SessionEnd::WriteFailed(e);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break SessionEnd::Shutdown,
            },
            pong = pong_rx.recv() => match pong {
                Some(frame) => {
                    if let Err(e) = writer.write_all(&frame).await {
                        break SessionEnd::WriteFailed(e);
                    }
                    pongs += 1;
                }
                // The reader stopped: the peer hung up, or sent something unreadable.
                None => match (&mut reader_task).await {
                    Ok(Some(e)) => break SessionEnd::ReadFailed(e),
                    _ => break SessionEnd::PeerClosed,
                },
            },
        }
    };
    reader_task.abort();
    SessionReport { end, pongs }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peer::message::PeerMessage;
    use crate::peer::protocol::{tm_ping::PingType, TmPing};
    use bytes::BytesMut;
    use tokio::io::DuplexStream;
    use tokio_util::codec::{Decoder, Encoder};

    /// A one-byte `mtMANIFESTS` frame.
    const MANIFEST: &[u8] = &[0, 0, 0, 1, 0, 2, 0xAA];

    /// A one-byte `mtVALIDATION` frame; `tag` tells them apart.
    fn validation(tag: u8) -> Vec<u8> {
        vec![0, 0, 0, 1, 0, 41, tag]
    }

    fn ping_frame(seq: u32) -> Vec<u8> {
        let ping = PeerMessage::Ping(TmPing {
            r#type: PingType::PtPing as i32,
            seq: Some(seq),
            ping_time: Some(99),
            net_time: None,
        });
        let mut buf = BytesMut::new();
        MessageCodec.encode(ping, &mut buf).unwrap();
        buf.to_vec()
    }

    /// The next whole frame the peer receives, or `None` once the connection is closed.
    async fn next_frame(peer: &mut DuplexStream) -> Option<Vec<u8>> {
        let read = async {
            let mut frame = vec![0u8; 6];
            peer.read_exact(&mut frame).await.ok()?;
            let len = u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]) as usize;
            let mut payload = vec![0u8; len];
            peer.read_exact(&mut payload).await.ok()?;
            frame.extend_from_slice(&payload);
            Some(frame)
        };
        tokio::time::timeout(Duration::from_secs(5), read).await.expect("peer read timed out")
    }

    fn pong_seq(frame: &[u8]) -> Option<u32> {
        match MessageCodec.decode(&mut BytesMut::from(frame)).unwrap() {
            Some(PeerMessage::Ping(p)) if p.r#type == PingType::PtPong as i32 => p.seq,
            _ => None,
        }
    }

    #[test]
    fn only_validations_are_remembered() {
        let t0 = Instant::now();
        let mut memory = RelayMemory::default();
        memory.forwarded(&validation(1), t0);
        memory.forwarded(MANIFEST, t0);
        assert_eq!(memory.resend_at(t0), Some(&validation(1)[..]));
    }

    #[test]
    fn a_validation_past_the_resend_window_is_not_resent() {
        let t0 = Instant::now();
        let mut memory = RelayMemory::default();
        memory.forwarded(&validation(1), t0);
        assert_eq!(memory.resend_at(t0 + RESEND_WITHIN - Duration::from_secs(1)), Some(&validation(1)[..]));
        assert_eq!(memory.resend_at(t0 + RESEND_WITHIN), None);
    }

    #[tokio::test]
    async fn opening_sends_the_manifest_then_the_recent_validation() {
        let (mut ours, mut peer) = tokio::io::duplex(1 << 16);
        let mut memory = RelayMemory::default();
        memory.forwarded(&validation(7), Instant::now());
        assert!(open_session(&mut ours, MANIFEST, &memory).await.unwrap());
        drop(ours);
        assert_eq!(next_frame(&mut peer).await.as_deref(), Some(MANIFEST));
        assert_eq!(next_frame(&mut peer).await, Some(validation(7)));
        assert_eq!(next_frame(&mut peer).await, None);
    }

    #[tokio::test]
    async fn opening_with_nothing_to_resend_sends_only_the_manifest() {
        let (mut ours, mut peer) = tokio::io::duplex(1 << 16);
        assert!(!open_session(&mut ours, MANIFEST, &RelayMemory::default()).await.unwrap());
        drop(ours);
        assert_eq!(next_frame(&mut peer).await.as_deref(), Some(MANIFEST));
        assert_eq!(next_frame(&mut peer).await, None);
    }

    #[tokio::test]
    async fn the_session_answers_the_peers_pings() {
        let (ours, mut peer) = tokio::io::duplex(1 << 16);
        let (tx, rx) = broadcast::channel::<Vec<u8>>(16);
        let mut memory = RelayMemory::default();
        let peer_side = async move {
            peer.write_all(&ping_frame(7)).await.unwrap();
            let pong = next_frame(&mut peer).await;
            drop(peer);
            pong
        };
        let (report, pong) = tokio::join!(run_session(ours, Vec::new(), rx, &mut memory), peer_side);
        assert_eq!(pong.as_deref().and_then(pong_seq), Some(7));
        assert_eq!(report.pongs, 1);
        assert!(matches!(report.end, SessionEnd::PeerClosed), "{:?}", report.end);
        drop(tx);
    }

    #[tokio::test]
    async fn a_ping_read_with_the_handshake_is_answered() {
        let (ours, mut peer) = tokio::io::duplex(1 << 16);
        let (_tx, rx) = broadcast::channel::<Vec<u8>>(16);
        let mut memory = RelayMemory::default();
        let peer_side = async move {
            let pong = next_frame(&mut peer).await;
            drop(peer);
            pong
        };
        let (report, pong) = tokio::join!(run_session(ours, ping_frame(9), rx, &mut memory), peer_side);
        assert_eq!(pong.as_deref().and_then(pong_seq), Some(9));
        assert_eq!(report.pongs, 1);
    }

    #[tokio::test]
    async fn the_session_forwards_the_broadcast_and_remembers_the_last_validation() {
        let (ours, mut peer) = tokio::io::duplex(1 << 16);
        let (tx, rx) = broadcast::channel::<Vec<u8>>(16);
        tx.send(validation(1)).unwrap();
        tx.send(MANIFEST.to_vec()).unwrap();
        let mut memory = RelayMemory::default();
        let peer_side = async move {
            let got = (next_frame(&mut peer).await, next_frame(&mut peer).await);
            drop(peer);
            got
        };
        let (report, got) = tokio::join!(run_session(ours, Vec::new(), rx, &mut memory), peer_side);
        assert_eq!(got, (Some(validation(1)), Some(MANIFEST.to_vec())));
        assert!(matches!(report.end, SessionEnd::PeerClosed), "{:?}", report.end);
        assert_eq!(memory.resend_at(Instant::now()), Some(&validation(1)[..]));
    }

    #[tokio::test]
    async fn an_unreadable_frame_ends_the_session() {
        let (ours, mut peer) = tokio::io::duplex(1 << 16);
        let (_tx, rx) = broadcast::channel::<Vec<u8>>(16);
        let mut memory = RelayMemory::default();
        peer.write_all(&[0xFF, 0xFF, 0xFF, 0xFF, 0, 3]).await.unwrap(); // claims a 4 GiB payload
        let report = run_session(ours, Vec::new(), rx, &mut memory).await;
        assert!(matches!(report.end, SessionEnd::ReadFailed(_)), "{:?}", report.end);
    }

    #[tokio::test]
    async fn the_session_ends_when_the_broadcast_closes() {
        let (ours, _peer) = tokio::io::duplex(1 << 16);
        let (tx, rx) = broadcast::channel::<Vec<u8>>(16);
        drop(tx);
        let report = run_session(ours, Vec::new(), rx, &mut RelayMemory::default()).await;
        assert!(matches!(report.end, SessionEnd::Shutdown), "{:?}", report.end);
    }

    #[test]
    fn a_refusing_hub_is_tried_less_and_less_often() {
        let secs: Vec<u64> = (1..=8).map(|n| retry_delay(n).as_secs()).collect();
        assert_eq!(secs, [10, 20, 40, 80, 160, 320, 600, 600]);
    }

    #[test]
    fn the_retry_delay_stays_capped_for_any_failure_count() {
        assert_eq!(retry_delay(u32::MAX), Duration::from_secs(600));
    }

    #[test]
    fn a_slot_moves_to_its_next_hub_after_each_failed_attempt() {
        let hubs = ["a:51235", "b:51235"];
        let tried: Vec<&str> = (0..4).map(|n| hub_for_attempt(&hubs, n)).collect();
        assert_eq!(tried, ["a:51235", "b:51235", "a:51235", "b:51235"]);
        assert_eq!(hub_for_attempt(&["only:51235"], 7), "only:51235");
    }

    /// A real handshake with every hub — run by hand after changing the list. A hub must take the
    /// connection or answer 503 (full, which a later attempt can get past); anything else means
    /// the hub cannot carry our validations.
    #[tokio::test]
    #[ignore = "network: opens a peer handshake to every relay hub"]
    async fn every_relay_hub_answers_a_peer_handshake() {
        use crate::peer::{handshake, identity::NodeIdentity};
        let _ = rustls::crypto::ring::default_provider().install_default();
        for hub in RELAY_HUBS.iter().flat_map(|slot| slot.iter()) {
            let id = NodeIdentity::generate().unwrap();
            let attempt = tokio::time::timeout(
                Duration::from_secs(15),
                handshake::outbound_handshake(hub, &id, handshake::NETWORK_ID_MAINNET),
            )
            .await;
            match attempt {
                Ok(Ok(_)) => {}
                Ok(Err(e)) if e.to_string().contains(" 503 ") => {}
                Ok(Err(e)) => panic!("{hub}: handshake failed: {e}"),
                Err(_) => panic!("{hub}: no answer within 15 s"),
            }
        }
    }
}
