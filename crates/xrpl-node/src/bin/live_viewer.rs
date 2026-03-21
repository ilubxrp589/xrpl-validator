//! Live XRPL peer message viewer — connects to testnet and streams
//! decoded messages to a web browser via Server-Sent Events.
//!
//! Run: cargo run -p xrpl-node --bin live_viewer
//! Open: http://localhost:3777

use std::time::{Duration, Instant};

use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::Html;
use axum::routing::get;
use axum::Router;
use bytes::BytesMut;
use futures::stream::Stream;
use tokio::io::AsyncReadExt;
use tokio::sync::broadcast;
use tokio::time::timeout;
use tokio_util::codec::Decoder;

use xrpl_node::peer::codec::MessageCodec;
use xrpl_node::peer::handshake;
use xrpl_node::peer::identity::NodeIdentity;
use xrpl_node::peer::message::PeerMessage;

const TESTNET_PEER: &str = "s.altnet.rippletest.net:51235";

#[derive(Clone, serde::Serialize)]
struct MessageEvent {
    seq: u64,
    time: f64,
    msg_type: String,
    detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    ledger_seq: Option<u32>,
}

#[tokio::main]
async fn main() {
    eprintln!("Starting XRPL Live Viewer...");

    let (tx, _) = broadcast::channel::<MessageEvent>(1000);
    let tx2 = tx.clone();

    // Spawn the peer connection task
    tokio::spawn(async move {
        loop {
            eprintln!("[peer] Connecting to {TESTNET_PEER}...");
            match run_peer_connection(&tx2).await {
                Ok(()) => eprintln!("[peer] Connection ended cleanly"),
                Err(e) => eprintln!("[peer] Error: {e}"),
            }
            eprintln!("[peer] Reconnecting in 5 seconds...");
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });

    // Web server
    let app = Router::new()
        .route("/", get(index_page))
        .route("/events", get(move || sse_handler(tx.clone())));

    let addr = "0.0.0.0:3777";
    eprintln!("[web] Listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn run_peer_connection(tx: &broadcast::Sender<MessageEvent>) -> Result<(), String> {
    let identity = NodeIdentity::generate().map_err(|e| format!("identity: {e}"))?;
    eprintln!("[peer] Node key: {}", identity.public_key_hex());

    let mut hs = timeout(
        Duration::from_secs(15),
        handshake::outbound_handshake(TESTNET_PEER, &identity, handshake::NETWORK_ID_TESTNET),
    )
    .await
    .map_err(|_| "handshake timeout".to_string())?
    .map_err(|e| format!("handshake: {e}"))?;

    eprintln!("[peer] Handshake successful!");

    let _ = tx.send(MessageEvent {
        seq: 0,
        time: 0.0,
        msg_type: "CONNECTED".into(),
        detail: format!("Handshake with {TESTNET_PEER}"),
        ledger_seq: None,
    });

    let mut codec = MessageCodec;
    let mut buf = BytesMut::with_capacity(65536);
    buf.extend_from_slice(&hs.remaining_bytes);

    let mut msg_seq: u64 = 0;
    let start = Instant::now();

    loop {
        // Decode from buffer
        loop {
            match codec.decode(&mut buf) {
                Ok(Some(msg)) => {
                    msg_seq += 1;
                    let elapsed = start.elapsed().as_secs_f64();

                    let (msg_type, detail, ledger_seq) = format_message(&msg);

                    let event = MessageEvent {
                        seq: msg_seq,
                        time: elapsed,
                        msg_type,
                        detail,
                        ledger_seq,
                    };

                    let _ = tx.send(event);
                    continue;
                }
                Ok(None) => break,
                Err(e) => {
                    eprintln!("[peer] Codec error: {e}");
                    return Err(format!("codec: {e}"));
                }
            }
        }

        // Read more from stream
        let mut chunk = vec![0u8; 32768];
        match timeout(Duration::from_secs(30), hs.stream.read(&mut chunk)).await {
            Ok(Ok(0)) => return Ok(()),
            Ok(Ok(n)) => buf.extend_from_slice(&chunk[..n]),
            Ok(Err(e)) => return Err(format!("read: {e}")),
            Err(_) => continue,
        }
    }
}

fn format_message(msg: &PeerMessage) -> (String, String, Option<u32>) {
    match msg {
        PeerMessage::StatusChange(sc) => {
            let status = sc.new_status.map(|s| match s {
                1 => "CONNECTING",
                2 => "CONNECTED",
                3 => "MONITORING",
                4 => "VALIDATING",
                5 => "SHUTTING",
                _ => "?",
            }).unwrap_or("?");
            let event = sc.new_event.map(|e| match e {
                1 => "CLOSING",
                2 => "ACCEPTED",
                3 => "SWITCHED",
                4 => "LOST_SYNC",
                _ => "?",
            }).unwrap_or("?");
            (
                "StatusChange".into(),
                format!("status={status} event={event}"),
                sc.ledger_seq,
            )
        }
        PeerMessage::Validation(v) => {
            let bytes = v.validation.len();
            ("Validation".into(), format!("{bytes}B signed validation"), None)
        }
        PeerMessage::Manifests(m) => {
            ("Manifests".into(), format!("{} validator manifests", m.list.len()), None)
        }
        PeerMessage::ValidatorListCollection(_) => {
            ("ValidatorList".into(), "UNL bootstrap".into(), None)
        }
        PeerMessage::Transaction(t) => {
            let bytes = t.raw_transaction.len();
            let status = t.status;
            ("Transaction".into(), format!("{bytes}B status={status}"), None)
        }
        PeerMessage::ProposeSet(p) => {
            let hash = hex::encode(&p.current_tx_hash[..std::cmp::min(8, p.current_tx_hash.len())]);
            ("Proposal".into(), format!("seq={} hash={hash}...", p.propose_seq), None)
        }
        PeerMessage::HaveTransactionSet(h) => {
            let hash = hex::encode(&h.hash[..std::cmp::min(8, h.hash.len())]);
            ("HaveTxSet".into(), format!("hash={hash}..."), None)
        }
        PeerMessage::Endpoints(e) => {
            ("Endpoints".into(), format!("{} peers", e.endpoints_v2.len()), None)
        }
        PeerMessage::GetLedger(g) => {
            ("GetLedger".into(), format!("seq={:?}", g.ledger_seq), None)
        }
        PeerMessage::LedgerData(d) => {
            ("LedgerData".into(), format!("{} nodes seq={}", d.nodes.len(), d.ledger_seq), None)
        }
        PeerMessage::Ping(p) => {
            let kind = if p.r#type == 0 { "ping" } else { "pong" };
            ("Ping".into(), format!("{kind} seq={:?}", p.seq), None)
        }
        other => (other.name().into(), String::new(), None),
    }
}

async fn index_page() -> Html<&'static str> {
    Html(include_str!("../../static/viewer.html"))
}

async fn sse_handler(
    tx: broadcast::Sender<MessageEvent>,
) -> Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>> {
    let rx = tx.subscribe();
    let stream = async_stream::stream! {
        let mut rx = rx;
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let data = serde_json::to_string(&event).unwrap_or_default();
                    yield Ok(Event::default().data(data));
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    yield Ok(Event::default().data(
                        format!("{{\"msg_type\":\"LAGGED\",\"detail\":\"skipped {n} messages\",\"seq\":0,\"time\":0}}")
                    ));
                }
                Err(_) => break,
            }
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::default())
}
