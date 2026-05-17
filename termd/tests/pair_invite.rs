use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

#[test]
fn pair_qr_prints_single_line_invite_code() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    listener.set_nonblocking(true).unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = accept_with_timeout(&listener);
        let mut request = Vec::new();
        let mut buffer = [0_u8; 1024];
        loop {
            let read = stream.read(&mut buffer).unwrap();
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }

        assert!(
            String::from_utf8_lossy(&request).starts_with("POST /local/pairing-token HTTP/1.1")
        );

        let body = "{\"token\":\"pair-token\",\"expires_at_ms\":1710000060000,\"server_id\":\"00000000-0000-0000-0000-000000000001\",\"daemon_public_key\":\"ed25519-v1:daemon-public\"}";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}"
        );
        stream.write_all(response.as_bytes()).unwrap();
    });

    let output = Command::new(env!("CARGO_BIN_EXE_termd"))
        .args([
            "pair",
            "--qr",
            "--url",
            &format!("http://127.0.0.1:{}", addr.port()),
        ])
        .output()
        .unwrap();

    server.join().unwrap();

    assert!(
        output.status.success(),
        "stdout={}",
        String::from_utf8_lossy(&output.stdout)
    );

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout
            .lines()
            .any(|line| line.starts_with("termd-pair:v1:")),
        "stdout={stdout}"
    );
    assert!(
        !stdout.contains("\"type\":\"termd_pairing_qr\""),
        "stdout={stdout}"
    );
}

#[test]
fn pair_qr_svg_writes_real_svg_and_prints_invite_code() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let svg_path = unique_svg_path("termd-pair");

    listener.set_nonblocking(true).unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = accept_with_timeout(&listener);
        let mut request = Vec::new();
        let mut buffer = [0_u8; 1024];
        loop {
            let read = stream.read(&mut buffer).unwrap();
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }

        assert!(
            String::from_utf8_lossy(&request).starts_with("POST /local/pairing-token HTTP/1.1")
        );

        let body = "{\"token\":\"pair-token\",\"expires_at_ms\":1710000060000,\"server_id\":\"00000000-0000-0000-0000-000000000001\",\"daemon_public_key\":\"ed25519-v1:daemon-public\"}";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}"
        );
        stream.write_all(response.as_bytes()).unwrap();
    });

    let output = Command::new(env!("CARGO_BIN_EXE_termd"))
        .args([
            "pair",
            "--qr-svg",
            svg_path.to_str().unwrap(),
            "--url",
            &format!("http://127.0.0.1:{}", addr.port()),
        ])
        .output()
        .unwrap();

    server.join().unwrap();

    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout
            .lines()
            .any(|line| line.starts_with("termd-pair:v1:")),
        "stdout={stdout}"
    );
    let svg = fs::read_to_string(&svg_path).unwrap();
    let _ = fs::remove_file(&svg_path);
    assert!(
        svg.starts_with(r#"<svg xmlns="http://www.w3.org/2000/svg""#),
        "{svg}"
    );
    assert!(svg.contains(r#"<path d=""#), "{svg}");
    assert!(svg.contains(r##"fill="#000000""##), "{svg}");
    assert!(svg.contains(r##"fill="#ffffff""##), "{svg}");
}

#[test]
fn invite_code_roundtrips_with_url_safe_base64() {
    let json = r#"{"type":"termd_pairing_qr","version":1,"token":"pair-token","server_id":"00000000-0000-0000-0000-000000000001","expires_at_ms":1710000060000,"daemon_public_key":"ed25519-v1:daemon-public"}"#;
    let invite = format!("termd-pair:v1:{}", URL_SAFE_NO_PAD.encode(json));

    let decoded = URL_SAFE_NO_PAD
        .decode(invite.trim_start_matches("termd-pair:v1:"))
        .unwrap();

    assert_eq!(String::from_utf8(decoded).unwrap(), json);
}

fn unique_svg_path(prefix: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "{prefix}-{}-{}.svg",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn accept_with_timeout(listener: &TcpListener) -> (std::net::TcpStream, std::net::SocketAddr) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match listener.accept() {
            Ok(result) => return result,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for termd pair to request local pairing token"
                );
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("failed to accept local pairing request: {error}"),
        }
    }
}
