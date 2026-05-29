use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use termd::auth::current_unix_timestamp_millis;
use termd::net::protocol::{JsonEnvelope, decode_payload, envelope_value};
use termd::net::{E2eeKeyPair, E2eeSession, E2eeSessionContext, E2eeSessionRole};
use termd_proto::{
    DeviceId, EncryptedFramePayload, Envelope, MessageType, Nonce, PairRequestPayload,
    PairingToken, ProtocolVersion, PublicKey, RelayClientId, RelayControlEnvelope,
    RouteHelloPayload, RouteReadyPayload, RouteRole, ServerId,
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
                connect_registered_socket(probe_url, probe_server_id, RouteRole::DaemonControl),
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
async fn relay_pairs_control_and_data_connections_as_raw_dumb_pipe() {
    let relay = RelayProcess::spawn().await;
    let server_id = ServerId::new();
    let mut daemon_control =
        connect_registered_socket(relay.ws_url(), server_id, RouteRole::DaemonControl)
            .await
            .expect("daemon control should connect");

    let (mut client, _) = connect_async(relay.ws_url().as_str())
        .await
        .expect("client should connect relay");
    send_route_hello(&mut client, server_id, RouteRole::Client)
        .await
        .expect("client route_hello should send");
    let (client_id, data_token) = expect_open_data(&mut daemon_control).await;
    assert!(
        timeout(Duration::from_millis(60), client.next())
            .await
            .is_err(),
        "client route_ready must wait until daemon data route is paired"
    );

    let (mut daemon_data, _) = connect_async(relay.ws_url().as_str())
        .await
        .expect("daemon data should connect relay");
    send_route_hello_with_data(
        &mut daemon_data,
        server_id,
        RouteRole::DaemonData,
        Some(client_id),
        Some(data_token),
    )
    .await
    .expect("daemon data route_hello should send");
    expect_route_ready(&mut daemon_data, server_id, RouteRole::DaemonData)
        .await
        .expect("daemon data should receive route_ready");
    expect_route_ready(&mut client, server_id, RouteRole::Client)
        .await
        .expect("client should receive route_ready after data pair");

    let (wire, _) = encrypted_pair_request_wire(server_id, RELAY_SECRET_SENTINEL);
    client
        .send(Message::Text(wire.clone()))
        .await
        .expect("client should send raw encrypted frame");
    assert_eq!(next_text(&mut daemon_data).await, wire);

    let binary = b"daemon-to-browser-binary-frame".to_vec();
    daemon_data
        .send(Message::Binary(binary.clone()))
        .await
        .expect("daemon data should send raw binary");
    assert_eq!(next_binary(&mut client).await, binary);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_forwards_real_encrypted_frame_without_plaintext_or_rewrite() {
    let relay = RelayProcess::spawn().await;
    let server_id = ServerId::new();
    let mut daemon_control =
        connect_registered_socket(relay.ws_url(), server_id, RouteRole::DaemonControl)
            .await
            .expect("daemon control side should connect");
    let (mut client, _) = connect_async(relay.ws_url().as_str())
        .await
        .expect("client side should connect");
    send_route_hello(&mut client, server_id, RouteRole::Client)
        .await
        .expect("client route_hello should send");
    let (client_id, data_token) = expect_open_data(&mut daemon_control).await;

    let (mut daemon_data, _) = connect_async(relay.ws_url().as_str())
        .await
        .expect("daemon data should connect");
    send_route_hello_with_data(
        &mut daemon_data,
        server_id,
        RouteRole::DaemonData,
        Some(client_id),
        Some(data_token),
    )
    .await
    .expect("daemon data route_hello should send");
    expect_route_ready(&mut daemon_data, server_id, RouteRole::DaemonData)
        .await
        .expect("daemon data should receive route_ready");
    expect_route_ready(&mut client, server_id, RouteRole::Client)
        .await
        .expect("client should receive route_ready");

    let (wire, mut daemon_e2ee) = encrypted_pair_request_wire(server_id, RELAY_SECRET_SENTINEL);
    assert!(wire.contains("encrypted_frame"));
    assert!(!wire.contains("pair_request"));
    assert!(!wire.contains(RELAY_SECRET_SENTINEL));

    client
        .send(Message::Text(wire.clone()))
        .await
        .expect("client should send encrypted frame");
    let received = next_text(&mut daemon_data).await;

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
    let mut daemon_control_a =
        connect_registered_socket(relay.ws_url(), server_a, RouteRole::DaemonControl)
            .await
            .expect("daemon A should connect");
    let mut daemon_control_b =
        connect_registered_socket(relay.ws_url(), server_b, RouteRole::DaemonControl)
            .await
            .expect("daemon B should connect");

    let (mut client_a, _) = connect_async(relay.ws_url().as_str())
        .await
        .expect("client A should connect");
    send_route_hello(&mut client_a, server_a, RouteRole::Client)
        .await
        .expect("client A route_hello should send");
    let (client_a_id, data_token_a) = expect_open_data(&mut daemon_control_a).await;

    let (mut client_b, _) = connect_async(relay.ws_url().as_str())
        .await
        .expect("client B should connect");
    send_route_hello(&mut client_b, server_b, RouteRole::Client)
        .await
        .expect("client B route_hello should send");
    let (client_b_id, data_token_b) = expect_open_data(&mut daemon_control_b).await;

    let (mut daemon_data_a, _) = connect_async(relay.ws_url().as_str())
        .await
        .expect("daemon data A should connect");
    send_route_hello_with_data(
        &mut daemon_data_a,
        server_a,
        RouteRole::DaemonData,
        Some(client_a_id),
        Some(data_token_a),
    )
    .await
    .expect("daemon data A route_hello should send");
    expect_route_ready(&mut daemon_data_a, server_a, RouteRole::DaemonData)
        .await
        .expect("daemon data A should receive route_ready");
    expect_route_ready(&mut client_a, server_a, RouteRole::Client)
        .await
        .expect("client A should receive route_ready");

    let (mut daemon_data_b, _) = connect_async(relay.ws_url().as_str())
        .await
        .expect("daemon data B should connect");
    send_route_hello_with_data(
        &mut daemon_data_b,
        server_b,
        RouteRole::DaemonData,
        Some(client_b_id),
        Some(data_token_b),
    )
    .await
    .expect("daemon data B route_hello should send");
    expect_route_ready(&mut daemon_data_b, server_b, RouteRole::DaemonData)
        .await
        .expect("daemon data B should receive route_ready");
    expect_route_ready(&mut client_b, server_b, RouteRole::Client)
        .await
        .expect("client B should receive route_ready");

    let (wire_a, _) = encrypted_pair_request_wire(server_a, RELAY_SECRET_SENTINEL);
    client_a
        .send(Message::Text(wire_a.clone()))
        .await
        .expect("client A should send encrypted frame");

    assert_eq!(next_text(&mut daemon_data_a).await, wire_a);
    assert!(
        timeout(Duration::from_millis(150), daemon_data_b.next())
            .await
            .is_err(),
        "server_id B daemon must not receive server_id A frame"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_forwards_business_shaped_text_and_binary_without_parsing() {
    let relay = RelayProcess::spawn().await;
    let server_id = ServerId::new();
    let mut daemon_control =
        connect_registered_socket(relay.ws_url(), server_id, RouteRole::DaemonControl)
            .await
            .expect("daemon control side should connect");
    let (mut client, _) = connect_async(relay.ws_url().as_str())
        .await
        .expect("client should connect");
    send_route_hello(&mut client, server_id, RouteRole::Client)
        .await
        .expect("client route_hello should send");
    let (client_id, data_token) = expect_open_data(&mut daemon_control).await;

    let (mut daemon_data, _) = connect_async(relay.ws_url().as_str())
        .await
        .expect("daemon data should connect");
    send_route_hello_with_data(
        &mut daemon_data,
        server_id,
        RouteRole::DaemonData,
        Some(client_id),
        Some(data_token),
    )
    .await
    .expect("daemon data route_hello should send");
    expect_route_ready(&mut daemon_data, server_id, RouteRole::DaemonData)
        .await
        .expect("daemon data should receive route_ready");
    expect_route_ready(&mut client, server_id, RouteRole::Client)
        .await
        .expect("client should receive route_ready");

    let text_payloads = [
        "{not-json session_data control_request pairing_token relay-secret-plaintext".to_owned(),
        r#"{"kind":"stream_open","method":"terminal.attach","payload":{"session_id":"session-a","watch_updates":true}}"#
            .to_owned(),
        r#"{"kind":"stream_chunk","method":"terminal.stdout","payload":{"kind":"snapshot","session_id":"session-a"}}"#
            .to_owned(),
        r#"{"kind":"stream_chunk","method":"terminal.stdin","payload":{"data_base64":"aW5wdXQ="}}"#
            .to_owned(),
        r#"{"kind":"request","method":"terminal.resize","payload":{"cols":120,"rows":40}}"#
            .to_owned(),
        r#"{"kind":"flow","ack":99,"credit":65536,"render_ack":true,"stale_session":true}"#
            .to_owned(),
    ];
    let binary_payloads = [
        b"\x00session_data\xffpairing_token\x00relay-secret-plaintext".to_vec(),
        b"\x01terminal.attach snapshot stdout stdin resize\x02flow ack credit render_ack stale_session".to_vec(),
    ];

    for text in text_payloads {
        client
            .send(Message::Text(text.clone()))
            .await
            .expect("client should send opaque text");
        assert_eq!(next_text(&mut daemon_data).await, text);

        let daemon_text = format!("daemon-to-client:{text}");
        daemon_data
            .send(Message::Text(daemon_text.clone()))
            .await
            .expect("daemon data should send text");
        assert_eq!(next_text(&mut client).await, daemon_text);
    }

    for binary in binary_payloads {
        client
            .send(Message::Binary(binary.clone()))
            .await
            .expect("client should send opaque binary");
        assert_eq!(next_binary(&mut daemon_data).await, binary);

        let mut daemon_binary = b"daemon-to-client:".to_vec();
        daemon_binary.extend_from_slice(&binary);
        daemon_data
            .send(Message::Binary(daemon_binary.clone()))
            .await
            .expect("daemon data should send binary");
        assert_eq!(next_binary(&mut client).await, daemon_binary);
    }
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
    let mut daemon_control = connect_registered_socket(
        format!("{}?relay_token=relay-secret-1", relay.ws_url()),
        server_id,
        RouteRole::DaemonControl,
    )
    .await
    .expect("authenticated daemon control should connect");
    let (mut client, _) = connect_async(format!("{}?relay_token=relay-secret-1", relay.ws_url()))
        .await
        .expect("authenticated client should connect");
    send_route_hello(&mut client, server_id, RouteRole::Client)
        .await
        .expect("authenticated client route_hello should send");
    let (client_id, data_token) = expect_open_data(&mut daemon_control).await;

    let (mut daemon_data, _) =
        connect_async(format!("{}?relay_token=relay-secret-1", relay.ws_url()))
            .await
            .expect("authenticated daemon data should connect");
    send_route_hello_with_data(
        &mut daemon_data,
        server_id,
        RouteRole::DaemonData,
        Some(client_id),
        Some(data_token),
    )
    .await
    .expect("authenticated daemon data route_hello should send");
    expect_route_ready(&mut daemon_data, server_id, RouteRole::DaemonData)
        .await
        .expect("daemon data should receive route_ready");
    expect_route_ready(&mut client, server_id, RouteRole::Client)
        .await
        .expect("client should receive route_ready");

    let business_shaped_text =
        "{\"type\":\"pair_request\",\"payload\":{\"token\":\"relay-secret-plaintext\"}}";
    client
        .send(Message::Text(business_shaped_text.to_owned()))
        .await
        .expect("authenticated client should send opaque text");

    assert_eq!(next_text(&mut daemon_data).await, business_shaped_text);
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
    send_route_hello_with_data(socket, server_id, role, None, None).await
}

async fn send_route_hello_with_data(
    socket: &mut RelaySocket,
    server_id: ServerId,
    role: RouteRole,
    client_id: Option<RelayClientId>,
    data_token: Option<Nonce>,
) -> Result<(), tokio_tungstenite::tungstenite::Error> {
    let route_hello = Envelope::new(
        MessageType::RouteHello,
        RouteHelloPayload {
            server_id,
            role,
            protocol_version: ProtocolVersion::default(),
            nonce: Nonce(format!("route-nonce-{}", Uuid::new_v4())),
            route_generation: None,
            client_id,
            data_token,
            timestamp_ms: current_unix_timestamp_millis(),
        },
    );
    let raw = serde_json::to_string(&route_hello).expect("route_hello should encode");
    socket.send(Message::Text(raw)).await
}

async fn expect_open_data(socket: &mut RelaySocket) -> (RelayClientId, Nonce) {
    loop {
        match timeout(Duration::from_secs(2), socket.next())
            .await
            .expect("relay control frame should arrive")
            .expect("daemon control should stay open")
            .expect("daemon control frame should be valid")
        {
            Message::Text(raw) => match serde_json::from_str::<RelayControlEnvelope>(&raw)
                .expect("relay control envelope should decode")
            {
                RelayControlEnvelope::OpenData {
                    client_id,
                    data_token,
                } => return (client_id, data_token),
                other => panic!("expected open_data, got {other:?}"),
            },
            Message::Ping(_) | Message::Pong(_) => continue,
            other => panic!("expected relay control text, got {other:?}"),
        }
    }
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
    loop {
        match timeout(Duration::from_secs(2), socket.next())
            .await
            .expect("relay frame should arrive before timeout")
            .expect("relay websocket should remain open")
            .expect("relay websocket frame should be valid")
        {
            Message::Text(text) => return text,
            Message::Ping(_) | Message::Pong(_) => continue,
            other => panic!("expected text frame, got {other:?}"),
        }
    }
}

async fn next_binary(socket: &mut RelaySocket) -> Vec<u8> {
    loop {
        match timeout(Duration::from_secs(2), socket.next())
            .await
            .expect("relay frame should arrive before timeout")
            .expect("relay websocket should remain open")
            .expect("relay websocket frame should be valid")
        {
            Message::Binary(bytes) => return bytes,
            Message::Ping(_) | Message::Pong(_) => continue,
            other => panic!("expected binary frame, got {other:?}"),
        }
    }
}

fn unused_listen_addr() -> SocketAddr {
    let listener =
        std::net::TcpListener::bind("127.0.0.1:0").expect("random relay port should bind");
    listener
        .local_addr()
        .expect("random relay port should expose local addr")
}
