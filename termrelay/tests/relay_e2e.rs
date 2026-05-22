use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use termd::auth::current_unix_timestamp_millis;
use termd::net::protocol::{JsonEnvelope, decode_payload, envelope_value};
use termd::net::{E2eeKeyPair, E2eeSession, E2eeSessionContext, E2eeSessionRole};
use termd_proto::{
    DeviceId, EncryptedFramePayload, Envelope, MessageType, Nonce, PairRequestPayload,
    PairingToken, ProtocolVersion, PublicKey, RelayMuxEnvelope, RelayOpaqueFrame,
    RouteHelloPayload, RouteReadyPayload, RouteRole, ServerId, decode_binary_relay_mux_envelope,
    encode_binary_relay_mux_envelope,
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
        Self::spawn_with_auth_token(None).await
    }

    async fn spawn_auth_required(token: &str) -> Self {
        Self::spawn_with_auth_token(Some(token)).await
    }

    async fn spawn_with_auth_token(auth_token: Option<&str>) -> Self {
        let addr = unused_listen_addr();
        let mut command = Command::new(env!("CARGO_BIN_EXE_termrelay"));
        command.args(["--listen", &addr.to_string()]);
        if let Some(auth_token) = auth_token {
            command.args(["--auth-token", auth_token]);
        }
        let child = command
            // relay 日志不参与不变量断言，避免测试依赖日志格式或落盘行为。
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("termrelay binary should spawn");
        let relay = Self { addr, child };

        relay.wait_until_accepting_websockets(auth_token).await;
        relay
    }

    async fn wait_until_accepting_websockets(&self, auth_token: Option<&str>) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);

        loop {
            let probe_server_id = ServerId::new();
            let probe_url = self.authenticated_url(self.ws_url(), auth_token);
            if let Ok(Ok(mut socket)) = timeout(
                Duration::from_millis(200),
                connect_registered_socket(probe_url, probe_server_id, RouteRole::DaemonMux),
            )
            .await
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

    fn ws_url(&self) -> String {
        format!("ws://{}/ws", self.addr)
    }

    fn authenticated_url(&self, url: String, auth_token: Option<&str>) -> String {
        match auth_token {
            Some(auth_token) => format!("{url}?relay_token={auth_token}"),
            None => url,
        }
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
    let mut daemon_mux = connect_registered_socket(relay.ws_url(), server_id, RouteRole::DaemonMux)
        .await
        .expect("daemon mux side should connect");
    let mut client = connect_registered_socket(relay.ws_url(), server_id, RouteRole::Client)
        .await
        .expect("client side should connect");
    let client_id = expect_client_connected(&mut daemon_mux).await;
    let (wire, mut daemon_e2ee) = encrypted_pair_request_wire(server_id, RELAY_SECRET_SENTINEL);

    assert!(wire.contains("encrypted_frame"));
    assert!(!wire.contains("pair_request"));
    assert!(!wire.contains(RELAY_SECRET_SENTINEL));

    client
        .send(Message::Text(wire.clone()))
        .await
        .expect("client should send encrypted frame");
    let received = expect_client_text(&mut daemon_mux, client_id).await;

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
    let mut daemon_a = connect_registered_socket(relay.ws_url(), server_a, RouteRole::DaemonMux)
        .await
        .expect("daemon A should connect");
    let mut daemon_b = connect_registered_socket(relay.ws_url(), server_b, RouteRole::DaemonMux)
        .await
        .expect("daemon B should connect");
    let mut client_a = connect_registered_socket(relay.ws_url(), server_a, RouteRole::Client)
        .await
        .expect("client A should connect");
    let _client_a_id = expect_client_connected(&mut daemon_a).await;
    let _client_b = connect_registered_socket(relay.ws_url(), server_b, RouteRole::Client)
        .await
        .expect("client B should connect");
    let _client_b_id = expect_client_connected(&mut daemon_b).await;
    let (wire_a, _daemon_a_e2ee) = encrypted_pair_request_wire(server_a, RELAY_SECRET_SENTINEL);

    client_a
        .send(Message::Text(wire_a.clone()))
        .await
        .expect("client A should send encrypted frame");

    assert!(matches!(
        next_mux(&mut daemon_a).await,
        RelayMuxEnvelope::ClientFrame {
            frame: RelayOpaqueFrame::Text { data },
            ..
        } if data == wire_a
    ));
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
    let mut daemon_mux = connect_registered_socket(relay.ws_url(), server_id, RouteRole::DaemonMux)
        .await
        .expect("daemon mux side should connect");
    let mut client = connect_registered_socket(relay.ws_url(), server_id, RouteRole::Client)
        .await
        .expect("client side should connect");
    let client_id = expect_client_connected(&mut daemon_mux).await;
    let text =
        "{not-json session_data control_request pairing_token relay-secret-plaintext".to_owned();
    let binary = b"\x00session_data\xffpairing_token\x00relay-secret-plaintext".to_vec();

    // 这些 frame 故意长得像业务 payload；relay 只能把它们当不透明字节转发。
    client
        .send(Message::Text(text.clone()))
        .await
        .expect("client should send opaque text");
    send_mux(
        &mut daemon_mux,
        RelayMuxEnvelope::DaemonFrame {
            client_id,
            frame: RelayOpaqueFrame::Binary {
                data_base64: base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    &binary,
                ),
            },
        },
    )
    .await;

    assert_eq!(expect_client_text(&mut daemon_mux, client_id).await, text);
    assert_eq!(next_binary(&mut client).await, binary);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_daemon_mux_data_forwards_output_but_not_client_lifecycle() {
    let relay = RelayProcess::spawn().await;
    let server_id = ServerId::new();
    let mut daemon_mux = connect_registered_socket(relay.ws_url(), server_id, RouteRole::DaemonMux)
        .await
        .expect("daemon control mux should connect");
    let mut daemon_data =
        connect_registered_socket(relay.ws_url(), server_id, RouteRole::DaemonMuxData)
            .await
            .expect("daemon data mux should connect after control mux");
    let mut client = connect_registered_socket(relay.ws_url(), server_id, RouteRole::Client)
        .await
        .expect("client should connect");
    let client_id = expect_client_connected(&mut daemon_mux).await;

    assert!(
        timeout(Duration::from_millis(150), daemon_data.next())
            .await
            .is_err(),
        "client lifecycle notifications must stay on daemon_mux control channel"
    );

    client
        .send(Message::Text("client-input".to_owned()))
        .await
        .expect("client should send input");
    assert_eq!(
        expect_client_text(&mut daemon_mux, client_id).await,
        "client-input"
    );
    assert!(
        timeout(Duration::from_millis(150), daemon_data.next())
            .await
            .is_err(),
        "client input must not be routed to daemon_mux_data"
    );

    send_mux(
        &mut daemon_data,
        RelayMuxEnvelope::DaemonFrame {
            client_id,
            frame: RelayOpaqueFrame::Text {
                data: "output-from-data-channel".to_owned(),
            },
        },
    )
    .await;

    assert_eq!(next_text(&mut client).await, "output-from-data-channel");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_keeps_daemon_mux_targeted_frames_within_server_id() {
    let relay = RelayProcess::spawn().await;
    let server_a = ServerId::new();
    let server_b = ServerId::new();
    let mut daemon_a = connect_registered_socket(relay.ws_url(), server_a, RouteRole::DaemonMux)
        .await
        .expect("daemon A should connect");
    let mut daemon_b = connect_registered_socket(relay.ws_url(), server_b, RouteRole::DaemonMux)
        .await
        .expect("daemon B should connect");
    let mut client_a = connect_registered_socket(relay.ws_url(), server_a, RouteRole::Client)
        .await
        .expect("client A should connect");
    let mut client_b = connect_registered_socket(relay.ws_url(), server_b, RouteRole::Client)
        .await
        .expect("client B should connect");
    let client_a_id = expect_client_connected(&mut daemon_a).await;
    let _client_b_id = expect_client_connected(&mut daemon_b).await;
    let targeted_frame = b"ciphertext-for-server-a-only".to_vec();

    send_mux(
        &mut daemon_a,
        RelayMuxEnvelope::DaemonFrame {
            client_id: client_a_id,
            frame: RelayOpaqueFrame::Binary {
                data_base64: base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    &targeted_frame,
                ),
            },
        },
    )
    .await;

    assert_eq!(next_binary(&mut client_a).await, targeted_frame);
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
    let mut daemon_mux = connect_registered_socket(relay.ws_url(), server_id, RouteRole::DaemonMux)
        .await
        .expect("daemon mux side should connect");
    let mut client_a = connect_registered_socket(relay.ws_url(), server_id, RouteRole::Client)
        .await
        .expect("client A should connect");
    let mut client_b = connect_registered_socket(relay.ws_url(), server_id, RouteRole::Client)
        .await
        .expect("client B should connect");

    let first_connected = next_mux(&mut daemon_mux).await;
    let second_connected = next_mux(&mut daemon_mux).await;
    let connected_ids = match (first_connected, second_connected) {
        (
            RelayMuxEnvelope::ClientConnected {
                client_id: first_id,
            },
            RelayMuxEnvelope::ClientConnected {
                client_id: second_id,
            },
        ) => [first_id, second_id],
        other => panic!("expected two client_connected envelopes, got {other:?}"),
    };

    let business_shaped_text =
        "{\"type\":\"pair_request\",\"payload\":{\"token\":\"relay-secret-plaintext\"}}";
    client_a
        .send(Message::Text(business_shaped_text.to_owned()))
        .await
        .expect("client A should send opaque text");

    // connect 通知是生命周期控制消息；不同 socket task 的调度顺序不等于测试里的 A/B 变量顺序。
    let RelayMuxEnvelope::ClientFrame {
        client_id: client_a_id,
        frame: RelayOpaqueFrame::Text {
            data: client_a_text,
        },
    } = next_mux(&mut daemon_mux).await
    else {
        panic!("expected client A frame");
    };
    assert!(connected_ids.contains(&client_a_id));
    assert_eq!(client_a_text, business_shaped_text);
    let client_b_id = connected_ids
        .into_iter()
        .find(|client_id| *client_id != client_a_id)
        .expect("client B id should be the other connected client");

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_auth_rejects_missing_and_wrong_transport_token() {
    let relay = RelayProcess::spawn_auth_required("relay-secret-1").await;

    assert!(connect_async(relay.ws_url()).await.is_err());
    assert!(
        connect_async(format!("{}?relay_token=wrong-secret", relay.ws_url()))
            .await
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_auth_allows_dumb_pipe_forwarding_with_correct_token() {
    let relay = RelayProcess::spawn_auth_required("relay-secret-1").await;
    let server_id = ServerId::new();
    let mut daemon_mux = connect_registered_socket(
        format!("{}?relay_token=relay-secret-1", relay.ws_url()),
        server_id,
        RouteRole::DaemonMux,
    )
    .await
    .expect("authenticated daemon mux should connect");
    let mut client = connect_registered_socket(
        format!("{}?relay_token=relay-secret-1", relay.ws_url()),
        server_id,
        RouteRole::Client,
    )
    .await
    .expect("authenticated client should connect");
    let client_id = expect_client_connected(&mut daemon_mux).await;
    let business_shaped_text =
        "{\"type\":\"pair_request\",\"payload\":{\"token\":\"relay-secret-plaintext\"}}";

    client
        .send(Message::Text(business_shaped_text.to_owned()))
        .await
        .expect("authenticated client should send opaque text");

    assert_eq!(
        expect_client_text(&mut daemon_mux, client_id).await,
        business_shaped_text
    );
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

async fn connect_registered_socket(
    url: String,
    server_id: ServerId,
    role: RouteRole,
) -> Result<RelaySocket, tokio_tungstenite::tungstenite::Error> {
    let (mut socket, _) = connect_async(url.as_str()).await?;
    send_route_hello(&mut socket, server_id, role).await?;
    expect_route_ready(&mut socket, server_id, role).await?;

    Ok(socket)
}

async fn send_route_hello(
    socket: &mut RelaySocket,
    server_id: ServerId,
    role: RouteRole,
) -> Result<(), tokio_tungstenite::tungstenite::Error> {
    let route_hello = Envelope::new(
        MessageType::RouteHello,
        RouteHelloPayload {
            server_id,
            role,
            protocol_version: ProtocolVersion::default(),
            nonce: Nonce(format!("route-nonce-{}", Uuid::new_v4())),
            route_generation: match role {
                RouteRole::DaemonMux | RouteRole::DaemonMuxData => {
                    Some(Nonce("relay-e2e-generation".to_owned()))
                }
                RouteRole::Client => None,
            },
            timestamp_ms: current_unix_timestamp_millis(),
        },
    );
    let raw = serde_json::to_string(&route_hello).expect("route_hello should encode");
    socket.send(Message::Text(raw)).await
}

async fn expect_route_ready(
    socket: &mut RelaySocket,
    server_id: ServerId,
    role: RouteRole,
) -> Result<(), tokio_tungstenite::tungstenite::Error> {
    let Some(message) = socket.next().await else {
        return Err(tokio_tungstenite::tungstenite::Error::ConnectionClosed);
    };
    let message = message?;
    let Message::Text(raw) = message else {
        return Err(tokio_tungstenite::tungstenite::Error::Protocol(
            tokio_tungstenite::tungstenite::error::ProtocolError::ResetWithoutClosingHandshake,
        ));
    };
    let ready: Envelope<RouteReadyPayload> =
        serde_json::from_str(&raw).expect("route_ready should decode");

    assert_eq!(ready.kind, MessageType::RouteReady);
    assert_eq!(ready.payload.server_id, server_id);
    assert_eq!(ready.payload.role, role);
    Ok(())
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
    match timeout(Duration::from_secs(2), socket.next())
        .await
        .expect("relay frame should arrive before timeout")
        .expect("relay websocket should remain open")
        .expect("relay websocket frame should be valid")
    {
        Message::Text(text) => {
            serde_json::from_str(&text).expect("relay mux envelope should decode")
        }
        Message::Binary(bytes) => decode_binary_relay_mux_envelope(&bytes)
            .expect("binary relay mux envelope should decode"),
        other => panic!("expected mux frame, got {other:?}"),
    }
}

async fn expect_client_connected(socket: &mut RelaySocket) -> termd_proto::RelayClientId {
    match next_mux(socket).await {
        RelayMuxEnvelope::ClientConnected { client_id } => client_id,
        other => panic!("expected client_connected envelope, got {other:?}"),
    }
}

async fn expect_client_text(
    socket: &mut RelaySocket,
    expected_client_id: termd_proto::RelayClientId,
) -> String {
    match next_mux(socket).await {
        RelayMuxEnvelope::ClientFrame {
            client_id,
            frame: RelayOpaqueFrame::Text { data },
        } if client_id == expected_client_id => data,
        other => panic!("expected client text frame, got {other:?}"),
    }
}

async fn send_mux(socket: &mut RelaySocket, envelope: RelayMuxEnvelope) {
    let raw = encode_binary_relay_mux_envelope(&envelope)
        .expect("relay mux envelope should encode as binary");
    socket
        .send(Message::Binary(raw))
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
