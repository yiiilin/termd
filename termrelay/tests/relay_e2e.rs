use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use termd_proto::ServerId;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::{sleep, timeout};

struct RelayProcess {
    addr: SocketAddr,
    child: Child,
}

impl RelayProcess {
    async fn spawn() -> Self {
        let addr = unused_listen_addr();
        let mut command = Command::new(env!("CARGO_BIN_EXE_termrelay"));
        command.args(["--listen", &addr.to_string(), "--allow-open-relay"]);
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("termrelay binary should spawn");
        let relay = Self { addr, child };
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
            ServerId::new().0
        ),
    )
    .await;

    assert!(response.starts_with("HTTP/1.1 503 Service Unavailable"));
    assert!(!response.contains("--http-tunnel"));
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
