use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use termd::auth::current_unix_timestamp_millis;
use termd::net::protocol::{JsonEnvelope, decode_payload, envelope_value};
use termd::net::{E2eeKeyPair, E2eeSession, E2eeSessionContext, E2eeSessionRole};
use termd_proto::{
    DeviceId, EncryptedFramePayload, MessageType, PairRequestPayload, PairingToken, PublicKey,
    RelayMuxEnvelope, RelayOpaqueFrame, ServerId,
};
use tokio::time::{sleep, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use uuid::Uuid;

const RELAY_SECRET_SENTINEL: &str = "relay-secret-plaintext";

type RelaySocket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

struct RelayProcess {
    addr: SocketAddr,
    child: Child,
}

impl RelayProcess {
    async fn spawn() -> Self {
        let addr = unused_listen_addr();
        let child = Command::new(env!("CARGO_BIN_EXE_termrelay"))
            .args(["--listen", &addr.to_string()])
            // relay 日志不参与不变量断言，避免测试依赖日志格式或落盘行为。
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("termrelay binary should spawn");
        let relay = Self { addr, child };

        relay.wait_until_accepting_websockets().await;
        relay
    }

    async fn wait_until_accepting_websockets(&self) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);

        loop {
            let probe_server_id = ServerId::new();
            let probe_url = self.daemon_url(probe_server_id);
            if let Ok(Ok((mut socket, _))) =
                timeout(Duration::from_millis(200), connect_async(probe_url)).await
            {
                let _ = socket.close(None).await;
                return;
            }

            assert!(
                tokio::time::Instant::now() < deadline,
                "termrelay did not accept websocket connections before timeout"
            );
            sleep(Duration::from_millis(25)).await;
        }
    }

    fn daemon_url(&self, server_id: ServerId) -> String {
        format!("ws://{}/ws/{}/daemon", self.addr, server_id.0)
    }

    fn daemon_mux_url(&self, server_id: ServerId) -> String {
        format!("ws://{}/ws/{}/daemon-mux", self.addr, server_id.0)
    }

    fn client_url(&self, server_id: ServerId) -> String {
        format!("ws://{}/ws/{}/client", self.addr, server_id.0)
    }
}

impl Drop for RelayProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_forwards_real_encrypted_frame_without_plaintext_or_rewrite() {
    let relay = RelayProcess::spawn().await;
    let server_id = ServerId::new();
    let (mut daemon, _) = connect_async(relay.daemon_url(server_id))
        .await
        .expect("daemon side should connect");
    let mut client = connect_registered_client(relay.client_url(server_id)).await;
    let (wire, mut daemon_e2ee) = encrypted_pair_request_wire(server_id, RELAY_SECRET_SENTINEL);

    assert!(wire.contains("encrypted_frame"));
    assert!(!wire.contains("pair_request"));
    assert!(!wire.contains(RELAY_SECRET_SENTINEL));

    client
        .send(Message::Text(wire.clone()))
        .await
        .expect("client should send encrypted frame");
    let received = next_text(&mut daemon).await;

    assert_eq!(received, wire);
    assert!(!received.contains("pair_request"));
    assert!(!received.contains(RELAY_SECRET_SENTINEL));

    let outer: JsonEnvelope =
        serde_json::from_str(&received).expect("forwarded encrypted frame should be JSON");
    let frame: EncryptedFramePayload =
        decode_payload(outer.payload).expect("outer payload should be encrypted_frame payload");
    let decrypted: JsonEnvelope = daemon_e2ee
        .decrypt_json_payload(&frame)
        .expect("daemon side should decrypt forwarded frame");
    let payload: PairRequestPayload =
        decode_payload(decrypted.payload).expect("inner pair_request payload should decode");

    assert_eq!(decrypted.kind, MessageType::PairRequest);
    assert_eq!(payload.token.0, RELAY_SECRET_SENTINEL);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_isolates_encrypted_frames_by_server_id() {
    let relay = RelayProcess::spawn().await;
    let server_a = ServerId::new();
    let server_b = ServerId::new();
    let (mut daemon_a, _) = connect_async(relay.daemon_url(server_a))
        .await
        .expect("daemon A should connect");
    let (mut daemon_b, _) = connect_async(relay.daemon_url(server_b))
        .await
        .expect("daemon B should connect");
    let mut client_a = connect_registered_client(relay.client_url(server_a)).await;
    let _client_b = connect_registered_client(relay.client_url(server_b)).await;
    let (wire_a, _daemon_a_e2ee) = encrypted_pair_request_wire(server_a, RELAY_SECRET_SENTINEL);

    client_a
        .send(Message::Text(wire_a.clone()))
        .await
        .expect("client A should send encrypted frame");

    assert_eq!(next_text(&mut daemon_a).await, wire_a);
    assert!(
        timeout(Duration::from_millis(150), daemon_b.next())
            .await
            .is_err(),
        "server_id B daemon must not receive server_id A frame"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_forwards_business_shaped_text_and_binary_without_parsing() {
    let relay = RelayProcess::spawn().await;
    let server_id = ServerId::new();
    let (mut daemon, _) = connect_async(relay.daemon_url(server_id))
        .await
        .expect("daemon side should connect");
    let mut client = connect_registered_client(relay.client_url(server_id)).await;
    let text =
        "{not-json session_data control_request pairing_token relay-secret-plaintext".to_owned();
    let binary = b"\x00session_data\xffpairing_token\x00relay-secret-plaintext".to_vec();

    // 这些 frame 故意长得像业务 payload；relay 只能把它们当不透明字节转发。
    client
        .send(Message::Text(text.clone()))
        .await
        .expect("client should send opaque text");
    daemon
        .send(Message::Binary(binary.clone()))
        .await
        .expect("daemon should send opaque binary");

    assert_eq!(next_text(&mut daemon).await, text);
    assert_eq!(next_binary(&mut client).await, binary);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_keeps_daemon_fanout_within_server_id() {
    let relay = RelayProcess::spawn().await;
    let server_a = ServerId::new();
    let server_b = ServerId::new();
    let (mut daemon_a, _) = connect_async(relay.daemon_url(server_a))
        .await
        .expect("daemon A should connect");
    let (_daemon_b, _) = connect_async(relay.daemon_url(server_b))
        .await
        .expect("daemon B should connect");
    let mut client_a = connect_registered_client(relay.client_url(server_a)).await;
    let mut client_b = connect_registered_client(relay.client_url(server_b)).await;
    let fanout_frame = b"ciphertext-for-server-a-only".to_vec();

    daemon_a
        .send(Message::Binary(fanout_frame.clone()))
        .await
        .expect("daemon A should send binary fanout frame");

    assert_eq!(next_binary(&mut client_a).await, fanout_frame);
    assert!(
        timeout(Duration::from_millis(150), client_b.next())
            .await
            .is_err(),
        "server_id B client must not receive server_id A daemon fanout"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_mux_routes_client_frames_and_targeted_daemon_responses() {
    let relay = RelayProcess::spawn().await;
    let server_id = ServerId::new();
    let (mut daemon_mux, _) = connect_async(relay.daemon_mux_url(server_id))
        .await
        .expect("daemon mux side should connect");
    let mut client_a = connect_registered_client(relay.client_url(server_id)).await;
    let mut client_b = connect_registered_client(relay.client_url(server_id)).await;

    let first_connected = next_mux(&mut daemon_mux).await;
    let second_connected = next_mux(&mut daemon_mux).await;
    let (client_a_id, client_b_id) = match (first_connected, second_connected) {
        (
            RelayMuxEnvelope::ClientConnected {
                client_id: first_id,
            },
            RelayMuxEnvelope::ClientConnected {
                client_id: second_id,
            },
        ) => (first_id, second_id),
        other => panic!("expected two client_connected envelopes, got {other:?}"),
    };

    let business_shaped_text =
        "{\"type\":\"pair_request\",\"payload\":{\"token\":\"relay-secret-plaintext\"}}";
    client_a
        .send(Message::Text(business_shaped_text.to_owned()))
        .await
        .expect("client A should send opaque text");

    assert_eq!(
        next_mux(&mut daemon_mux).await,
        RelayMuxEnvelope::ClientFrame {
            client_id: client_a_id,
            frame: RelayOpaqueFrame::Text {
                data: business_shaped_text.to_owned(),
            },
        }
    );

    send_mux(
        &mut daemon_mux,
        RelayMuxEnvelope::DaemonFrame {
            client_id: client_a_id,
            frame: RelayOpaqueFrame::Text {
                data: "daemon-response-for-a".to_owned(),
            },
        },
    )
    .await;

    assert_eq!(next_text(&mut client_a).await, "daemon-response-for-a");
    assert!(
        timeout(Duration::from_millis(150), client_b.next())
            .await
            .is_err(),
        "client B must not receive client A mux response"
    );

    client_b
        .send(Message::Binary(vec![9, 8, 7]))
        .await
        .expect("client B should send opaque binary");
    assert_eq!(
        next_mux(&mut daemon_mux).await,
        RelayMuxEnvelope::ClientFrame {
            client_id: client_b_id,
            frame: RelayOpaqueFrame::Binary {
                data_base64: "CQgH".to_owned(),
            },
        }
    );

    send_mux(
        &mut daemon_mux,
        RelayMuxEnvelope::DaemonFrame {
            client_id: client_b_id,
            frame: RelayOpaqueFrame::Binary {
                data_base64: "AQID".to_owned(),
            },
        },
    )
    .await;
    assert_eq!(next_binary(&mut client_b).await, vec![1, 2, 3]);
}

fn encrypted_pair_request_wire(server_id: ServerId, token: &str) -> (String, E2eeSession) {
    let device_id = DeviceId::new();
    let daemon_keypair = E2eeKeyPair::generate();
    let device_keypair = E2eeKeyPair::generate();
    let context = E2eeSessionContext::new(
        server_id,
        device_id,
        daemon_keypair.public_key(),
        device_keypair.public_key(),
    );
    let mut device_e2ee = E2eeSession::new(
        E2eeSessionRole::Device,
        &device_keypair,
        daemon_keypair.public_key(),
        context.clone(),
    )
    .expect("device E2EE session should be created");
    let daemon_e2ee = E2eeSession::new(
        E2eeSessionRole::Daemon,
        &daemon_keypair,
        device_keypair.public_key(),
        context,
    )
    .expect("daemon E2EE session should be created");
    let inner = envelope_value(
        MessageType::PairRequest,
        PairRequestPayload {
            device_id,
            device_public_key: PublicKey("ed25519-v1:relay-test-device".to_owned()),
            token: PairingToken(token.to_owned()),
            nonce: termd_proto::Nonce(format!("nonce-{}", Uuid::new_v4())),
            timestamp_ms: current_unix_timestamp_millis(),
        },
    )
    .expect("inner pair_request should serialize");
    let frame = device_e2ee
        .encrypt_json_payload(&inner)
        .expect("inner pair_request should encrypt");
    let outer = envelope_value(MessageType::EncryptedFrame, frame)
        .expect("outer encrypted_frame should serialize");
    let wire = serde_json::to_string(&outer).expect("outer encrypted_frame should encode as JSON");

    (wire, daemon_e2ee)
}

async fn connect_registered_client(url: String) -> RelaySocket {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);

    loop {
        if let Ok(Ok((mut socket, _))) =
            timeout(Duration::from_millis(200), connect_async(url.as_str())).await
        {
            let probe = b"relay-registration-probe".to_vec();
            if socket.send(Message::Ping(probe.clone())).await.is_ok() {
                match timeout(Duration::from_millis(200), socket.next()).await {
                    Ok(Some(Ok(Message::Pong(payload)))) if payload == probe => return socket,
                    _ => {}
                }
            }

            let _ = socket.close(None).await;
        }

        assert!(
            tokio::time::Instant::now() < deadline,
            "relay client did not register before timeout"
        );
        sleep(Duration::from_millis(25)).await;
    }
}

async fn next_text(socket: &mut RelaySocket) -> String {
    match timeout(Duration::from_secs(2), socket.next())
        .await
        .expect("relay frame should arrive before timeout")
        .expect("relay websocket should remain open")
        .expect("relay websocket frame should be valid")
    {
        Message::Text(text) => text,
        other => panic!("expected text frame, got {other:?}"),
    }
}

async fn next_binary(socket: &mut RelaySocket) -> Vec<u8> {
    match timeout(Duration::from_secs(2), socket.next())
        .await
        .expect("relay frame should arrive before timeout")
        .expect("relay websocket should remain open")
        .expect("relay websocket frame should be valid")
    {
        Message::Binary(bytes) => bytes,
        other => panic!("expected binary frame, got {other:?}"),
    }
}

async fn next_mux(socket: &mut RelaySocket) -> RelayMuxEnvelope {
    serde_json::from_str(&next_text(socket).await).expect("relay mux envelope should decode")
}

async fn send_mux(socket: &mut RelaySocket, envelope: RelayMuxEnvelope) {
    let raw = serde_json::to_string(&envelope).expect("relay mux envelope should encode");
    socket
        .send(Message::Text(raw))
        .await
        .expect("daemon mux should send envelope");
}

fn unused_listen_addr() -> SocketAddr {
    let listener =
        std::net::TcpListener::bind("127.0.0.1:0").expect("random relay port should bind");
    listener
        .local_addr()
        .expect("random relay port should expose local addr")
}
