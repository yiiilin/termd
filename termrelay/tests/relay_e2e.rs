use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use termd::auth::current_unix_timestamp_millis;
use termd::net::protocol::{JsonEnvelope, decode_payload, envelope_value};
use termd::net::{E2eeKeyPair, E2eeSession, E2eeSessionContext, E2eeSessionRole};
use termd_proto::{
    DeviceId, EncryptedFramePayload, Envelope, ErrorPayload, MessageType, Nonce,
    PairRequestPayload, PairingToken, ProtocolVersion, PublicKey, RelayClientId,
    RelayControlEnvelope, RouteHelloPayload, RouteReadyPayload, RouteRole, ServerId,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::{sleep, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use uuid::Uuid;

const RELAY_SECRET_SENTINEL: &str = "relay-secret-plaintext";
const RELAY_AUTH_TOKEN: &str = "relay-secret-1-with-enough-length";
const RELAY_WRONG_AUTH_TOKEN: &str = "relay-secret-2-with-enough-length";

type RelaySocket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

struct RelayProcess {
    addr: SocketAddr,
    child: Child,
}

impl RelayProcess {
    async fn spawn() -> Self {
        Self::spawn_with_auth_token(None).await
    }

    async fn spawn_with_http_tunnel() -> Self {
        Self::spawn_with_options(None, true).await
    }

    async fn spawn_auth_required(token: &str) -> Self {
        Self::spawn_with_auth_token(Some(token)).await
    }

    async fn spawn_with_auth_token(auth_token: Option<&str>) -> Self {
        Self::spawn_with_options(auth_token, false).await
    }

    async fn spawn_with_options(auth_token: Option<&str>, http_tunnel: bool) -> Self {
        let addr = unused_listen_addr();
        let mut command = Command::new(env!("CARGO_BIN_EXE_termrelay"));
        command.args(["--listen", &addr.to_string()]);
        command.arg("--allow-open-relay");
        if let Some(auth_token) = auth_token {
            command.args(["--auth-token", auth_token]);
        }
        if http_tunnel {
            command.arg("--http-tunnel");
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
async fn relay_http_file_tunnel_is_disabled_by_default() {
    let relay = RelayProcess::spawn().await;
    let response = raw_http_request(
        relay.addr,
        &format!(
            "POST /api/files/upload/init HTTP/1.1\r\nHost: {}\r\nx-termd-server-id: {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            relay.addr,
            ServerId::new().0
        ),
    )
    .await;

    assert!(response.starts_with("HTTP/1.1 501 Not Implemented"));
    assert!(response.contains("--http-tunnel"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_http_file_tunnel_flag_reaches_binary_router() {
    let relay = RelayProcess::spawn_with_http_tunnel().await;
    let response = raw_http_request(
        relay.addr,
        &format!(
            "POST /api/files/upload/init HTTP/1.1\r\nHost: {}\r\nx-termd-server-id: {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            relay.addr,
            ServerId::new().0
        ),
    )
    .await;

    // 中文注释：这里不启动 daemon control，503 说明请求已经通过 --http-tunnel 进入
    // 兼容 tunnel 路径；若 flag 没接到 router，会继续返回默认禁用的 501。
    assert!(response.starts_with("HTTP/1.1 503 Service Unavailable"));
    assert!(!response.contains("--http-tunnel"));
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
    expect_route_ready(&mut client, server_id, RouteRole::Client)
        .await
        .expect("client should receive route_ready before data pair");

    let (early_wire, _) = encrypted_pair_request_wire(server_id, RELAY_SECRET_SENTINEL);
    client
        .send(Message::Text(early_wire.clone()))
        .await
        .expect("client should send raw encrypted frame before data pair");

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
    assert_eq!(next_text(&mut daemon_data).await, early_wire);

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
async fn relay_rejects_daemon_data_without_client_assignment() {
    let relay = RelayProcess::spawn().await;
    let server_id = ServerId::new();
    let mut daemon_control =
        connect_registered_socket(relay.ws_url(), server_id, RouteRole::DaemonControl)
            .await
            .expect("daemon control should connect");

    let (mut unassigned_daemon_data, _) = connect_async(relay.ws_url().as_str())
        .await
        .expect("daemon data should connect relay");
    send_route_hello(
        &mut unassigned_daemon_data,
        server_id,
        RouteRole::DaemonData,
    )
    .await
    .expect("daemon data route_hello should send");
    expect_route_error(&mut unassigned_daemon_data, "relay_data_route_invalid").await;
    expect_socket_closed(&mut unassigned_daemon_data, Duration::from_secs(2)).await;

    let (mut client, _) = connect_async(relay.ws_url().as_str())
        .await
        .expect("client should connect relay");
    send_route_hello(&mut client, server_id, RouteRole::Client)
        .await
        .expect("client route_hello should send");
    let (client_id, data_token) = expect_open_data(&mut daemon_control).await;
    expect_route_ready(&mut client, server_id, RouteRole::Client)
        .await
        .expect("client should receive route_ready before data pair");

    let wire = "encrypted-client-frame-before-one-to-one-data-connect".to_owned();
    client
        .send(Message::Text(wire.clone()))
        .await
        .expect("client should send raw frame before data pair");

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
    assert_eq!(next_text(&mut daemon_data).await, wire);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_requires_matching_client_token_for_daemon_data() {
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
    expect_route_ready(&mut client, server_id, RouteRole::Client)
        .await
        .expect("client should receive route_ready before data pair");

    let (mut wrong_daemon_data, _) = connect_async(relay.ws_url().as_str())
        .await
        .expect("wrong daemon data should connect relay");
    send_route_hello_with_data(
        &mut wrong_daemon_data,
        server_id,
        RouteRole::DaemonData,
        Some(client_id),
        Some(Nonce("wrong-one-to-one-data-token".to_owned())),
    )
    .await
    .expect("wrong daemon data route_hello should send");
    expect_route_error(&mut wrong_daemon_data, "relay_data_route_rejected").await;
    expect_socket_closed(&mut wrong_daemon_data, Duration::from_secs(2)).await;

    let wire = "encrypted-client-frame-after-token-reject".to_owned();
    client
        .send(Message::Text(wire.clone()))
        .await
        .expect("client should send raw frame before correct data pair");

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
    assert_eq!(next_text(&mut daemon_data).await, wire);

    let daemon_frame = b"daemon-frame-through-matched-one-to-one-pipe".to_vec();
    daemon_data
        .send(Message::Binary(daemon_frame.clone()))
        .await
        .expect("daemon data should send raw binary");
    assert_eq!(next_binary(&mut client).await, daemon_frame);

    client.close(None).await.expect("client close should send");
    match timeout(Duration::from_secs(2), daemon_data.next())
        .await
        .expect("relay should close old daemon data pipe")
    {
        Some(Ok(Message::Close(_))) | Some(Err(_)) | None => {}
        Some(Ok(other)) => {
            panic!("expected daemon data close after client disconnect, got {other:?}")
        }
    }

    let (mut second_client, _) = connect_async(relay.ws_url().as_str())
        .await
        .expect("second client should connect relay");
    send_route_hello(&mut second_client, server_id, RouteRole::Client)
        .await
        .expect("second client route_hello should send");
    expect_route_ready(&mut second_client, server_id, RouteRole::Client)
        .await
        .expect("second client should receive route_ready");
    let (second_client_id, _second_token) = expect_open_data(&mut daemon_control).await;
    assert!(
        second_client_id.0 > client_id.0,
        "新 client 必须拿到新的 OpenData 标识"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_forwards_business_type_envelopes_without_interpreting_them() {
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
        .expect("client should receive route_ready");

    // 中文注释：这些看起来像业务 envelope，但 route prelude 之后都只是 opaque WebSocket frame。
    let business_frames = [
        r#"{"type":"auth","payload":{"device_id":"dev-a","signature":"sig"}}"#,
        r#"{"type":"session_data","payload":{"session_id":"session-a","data_base64":"aWQ="}}"#,
        r#"{"type":"control_request","payload":{"session_id":"session-a"}}"#,
    ];
    for frame in business_frames {
        client
            .send(Message::Text(frame.to_owned()))
            .await
            .expect("client should send business-shaped text");
        assert_eq!(next_text(&mut daemon_data).await, frame);
    }

    let binary = b"\x00auth\x00session_data\x00control_request\x00".to_vec();
    client
        .send(Message::Binary(binary.clone()))
        .await
        .expect("client should send business-shaped binary");
    assert_eq!(next_binary(&mut daemon_data).await, binary);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_forwards_large_terminal_snapshot_sized_binary_frame() {
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

    // 真实终端 snapshot 是 E2EE 后的单个 opaque binary WebSocket frame。
    // relay 不能解析后分片，因此传输层必须允许 MB 级重绘帧原样通过。
    let snapshot_sized_frame = vec![0x5a; 6 * 1024 * 1024];
    daemon_data
        .send(Message::Binary(snapshot_sized_frame.clone()))
        .await
        .expect("daemon data should send large terminal snapshot frame");
    assert_eq!(next_binary(&mut client).await, snapshot_sized_frame);
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
    let relay = RelayProcess::spawn_auth_required(RELAY_AUTH_TOKEN).await;

    assert!(connect_async(relay.ws_url()).await.is_err());
    assert!(
        connect_async(format!(
            "{}?relay_token={RELAY_WRONG_AUTH_TOKEN}",
            relay.ws_url()
        ))
        .await
        .is_err()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_auth_allows_dumb_pipe_forwarding_with_correct_token() {
    let relay = RelayProcess::spawn_auth_required(RELAY_AUTH_TOKEN).await;
    let server_id = ServerId::new();
    let mut daemon_control = connect_registered_socket(
        format!("{}?relay_token={RELAY_AUTH_TOKEN}", relay.ws_url()),
        server_id,
        RouteRole::DaemonControl,
    )
    .await
    .expect("authenticated daemon control should connect");
    let (mut client, _) =
        connect_async(format!("{}?relay_token={RELAY_AUTH_TOKEN}", relay.ws_url()))
            .await
            .expect("authenticated client should connect");
    send_route_hello(&mut client, server_id, RouteRole::Client)
        .await
        .expect("authenticated client route_hello should send");
    let (client_id, data_token) = expect_open_data(&mut daemon_control).await;

    let (mut daemon_data, _) =
        connect_async(format!("{}?relay_token={RELAY_AUTH_TOKEN}", relay.ws_url()))
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
    let route_generation = match role {
        RouteRole::DaemonControl | RouteRole::DaemonData => {
            Some(Nonce(format!("relay-e2e-route-generation-{}", server_id.0)))
        }
        RouteRole::Client | RouteRole::DaemonMux => None,
    };
    let route_hello = Envelope::new(
        MessageType::RouteHello,
        RouteHelloPayload {
            server_id,
            role,
            protocol_version: ProtocolVersion::default(),
            nonce: Nonce(format!("route-nonce-{}", Uuid::new_v4())),
            admission: None,
            route_generation,
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
            Message::Ping(payload) => {
                let _ = socket.send(Message::Pong(payload.clone())).await;
            }
            Message::Pong(_) => continue,
            other => panic!("expected relay control text, got {other:?}"),
        }
    }
}

async fn expect_route_error(socket: &mut RelaySocket, code: &str) {
    loop {
        match timeout(Duration::from_secs(2), socket.next())
            .await
            .expect("relay error should arrive before timeout")
            .expect("relay websocket should remain open until error is sent")
            .expect("relay websocket error frame should be valid")
        {
            Message::Text(raw) => {
                let error: Envelope<ErrorPayload> =
                    serde_json::from_str(&raw).expect("relay error should decode");
                assert_eq!(error.kind, MessageType::Error);
                assert_eq!(error.payload.code, code);
                return;
            }
            Message::Ping(payload) => {
                let _ = socket.send(Message::Pong(payload)).await;
            }
            Message::Pong(_) => continue,
            other => panic!("expected relay error text, got {other:?}"),
        }
    }
}

async fn expect_route_ready(
    socket: &mut RelaySocket,
    server_id: ServerId,
    role: RouteRole,
) -> Result<(), tokio_tungstenite::tungstenite::Error> {
    let Some(message) = timeout(Duration::from_secs(2), socket.next())
        .await
        .expect("route_ready should arrive before timeout")
    else {
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

async fn expect_socket_closed(socket: &mut RelaySocket, wait: Duration) {
    timeout(wait, async {
        loop {
            match socket.next().await {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                Some(Ok(Message::Ping(payload))) => {
                    let _ = socket.send(Message::Pong(payload)).await;
                }
                Some(Ok(Message::Pong(_))) => {}
                Some(Ok(other)) => panic!("expected relay websocket close, got {other:?}"),
            }
        }
    })
    .await
    .expect("relay websocket should close before timeout");
}

async fn raw_http_request(addr: SocketAddr, request: &str) -> String {
    let mut stream = tokio::net::TcpStream::connect(addr)
        .await
        .expect("relay HTTP port should accept TCP");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("HTTP request should write");
    let mut response = Vec::new();
    timeout(Duration::from_secs(2), stream.read_to_end(&mut response))
        .await
        .expect("HTTP response should finish before timeout")
        .expect("HTTP response should read");
    String::from_utf8(response).expect("HTTP response should be UTF-8")
}

fn unused_listen_addr() -> SocketAddr {
    let listener =
        std::net::TcpListener::bind("127.0.0.1:0").expect("random relay port should bind");
    listener
        .local_addr()
        .expect("random relay port should expose local addr")
}
