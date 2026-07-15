use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use termd_proto::{
    Envelope, ErrorPayload, MessageType, Nonce, ProtocolVersion, RelayAdmissionPayload,
    RouteHelloPayload, RouteReadyPayload, RouteRole, ServerId, UnixTimestampMillis,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::{sleep, timeout};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

const RELAY_SETUP_TOKEN: &str = "relay-e2e-setup-token-secret";
const RELAY_DAEMON_TOKEN: &str = "relay-e2e-daemon-token-secret";

struct RelayProcess {
    addr: SocketAddr,
    child: Child,
    setup_token_file: PathBuf,
    registry_file: PathBuf,
    server_id: ServerId,
}

impl RelayProcess {
    async fn spawn() -> Self {
        let addr = unused_listen_addr();
        let server_id = ServerId::new();
        let unique = format!("{}-{}", std::process::id(), ServerId::new().0);
        let setup_token_file =
            std::env::temp_dir().join(format!("termrelay-e2e-setup-{unique}.token"));
        let registry_file =
            std::env::temp_dir().join(format!("termrelay-e2e-registry-{unique}.json"));
        fs::write(&setup_token_file, format!("{RELAY_SETUP_TOKEN}\n"))
            .expect("setup token fixture should write");
        fs::write(
            &registry_file,
            format!(
                "{{\"daemons\":[{{\"server_id\":\"{}\",\"token\":\"{}\"}}]}}\n",
                server_id.0, RELAY_DAEMON_TOKEN
            ),
        )
        .expect("daemon registry fixture should write");
        let mut command = Command::new(env!("CARGO_BIN_EXE_termrelay"));
        command
            .arg("--listen")
            .arg(addr.to_string())
            .arg("--setup-token-file")
            .arg(&setup_token_file)
            .arg("--daemon-registry")
            .arg(&registry_file);
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("termrelay binary should spawn");
        let relay = Self {
            addr,
            child,
            setup_token_file,
            registry_file,
            server_id,
        };
        relay.wait_until_accepting_http().await;
        relay
    }

    async fn wait_until_accepting_http(&self) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if tokio::net::TcpStream::connect(self.addr).await.is_ok() {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "termrelay did not accept HTTP connections before timeout"
            );
            sleep(Duration::from_millis(25)).await;
        }
    }
}

impl Drop for RelayProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_file(&self.setup_token_file);
        let _ = fs::remove_file(&self.registry_file);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_v070_http_file_tunnel_is_available_by_default() {
    let relay = RelayProcess::spawn().await;
    let response = raw_http_request(
        relay.addr,
        &format!(
            "POST /api/files/uploads HTTP/1.1\r\nHost: {}\r\nx-termd-server-id: {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            relay.addr,
            relay.server_id.0
        ),
    )
    .await;

    assert!(response.starts_with("HTTP/1.1 401 Unauthorized"));
    assert!(!response.contains("--http-tunnel"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trusted_daemon_control_reports_connected_for_registered_token() {
    let relay = RelayProcess::spawn().await;

    let mut rejected = connect_daemon_control(&relay, "wrong-relay-e2e-daemon-token").await;
    let error: Envelope<ErrorPayload> = serde_json::from_str(&next_text(&mut rejected).await)
        .expect("relay admission error should be standard JSON");
    assert_eq!(error.kind, MessageType::Error);
    assert_eq!(error.payload.code, "relay_admission_rejected");

    let mut daemon_control = connect_daemon_control(&relay, RELAY_DAEMON_TOKEN).await;
    let ready: Envelope<RouteReadyPayload> =
        serde_json::from_str(&next_text(&mut daemon_control).await)
            .expect("route_ready should be standard JSON");
    assert_eq!(ready.kind, MessageType::RouteReady);
    assert_eq!(ready.payload.server_id, relay.server_id);
    assert_eq!(ready.payload.role, RouteRole::DaemonControl);

    let body = serde_json::json!({"server_id": relay.server_id}).to_string();
    let response = raw_http_request(
        relay.addr,
        &format!(
            "POST /api/relay/daemon/status HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nx-termd-relay-setup-token: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            relay.addr,
            RELAY_SETUP_TOKEN,
            body.len(),
            body
        ),
    )
    .await;

    assert!(response.starts_with("HTTP/1.1 200 OK"));
    assert!(
        response
            .to_ascii_lowercase()
            .contains("content-type: application/json")
    );
    let (_, response_body) = response
        .split_once("\r\n\r\n")
        .expect("HTTP response should contain a body separator");
    let status: serde_json::Value =
        serde_json::from_str(response_body).expect("daemon status response should be JSON");
    assert_eq!(status["server_id"], relay.server_id.0.to_string());
    assert_eq!(status["connected"], true);
}

async fn connect_daemon_control(
    relay: &RelayProcess,
    daemon_token: &str,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let (mut socket, _) = connect_async(format!("ws://{}/ws", relay.addr))
        .await
        .expect("relay websocket should connect");
    let hello = Envelope::new(
        MessageType::RouteHello,
        RouteHelloPayload {
            server_id: relay.server_id,
            role: RouteRole::DaemonControl,
            protocol_version: ProtocolVersion::default(),
            nonce: Nonce(format!("relay-e2e-route-nonce-{}", ServerId::new().0)),
            admission: Some(RelayAdmissionPayload::Daemon {
                token: daemon_token.to_owned(),
            }),
            route_generation: Some(Nonce(format!(
                "relay-e2e-route-generation-{}",
                ServerId::new().0
            ))),
            client_id: None,
            data_token: None,
            timestamp_ms: UnixTimestampMillis(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("system clock should follow Unix epoch")
                    .as_millis() as u64,
            ),
        },
    );
    socket
        .send(Message::Text(
            serde_json::to_string(&hello).expect("route_hello should encode"),
        ))
        .await
        .expect("route_hello should send");
    socket
}

async fn next_text(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> String {
    loop {
        match timeout(Duration::from_secs(2), socket.next())
            .await
            .expect("relay should answer before timeout")
            .expect("relay websocket should stay open until its response")
            .expect("relay websocket response should be valid")
        {
            Message::Text(raw) => return raw,
            Message::Ping(_) | Message::Pong(_) => continue,
            other => panic!("expected relay text response, got {other:?}"),
        }
    }
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
