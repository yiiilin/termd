use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::thread;

#[test]
fn pair_qr_prints_single_line_invite_code() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
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

        let body = "{\"token\":\"pair-token\",\"expires_at_ms\":1710000060000,\"server_id\":\"00000000-0000-0000-0000-000000000001\"}";
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
            "--ws-url",
            "wss://relay.example/ws/{server_id}/client",
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
fn invite_code_roundtrips_with_url_safe_base64() {
    let json = r#"{"type":"termd_pairing_qr","version":1,"ws_url":"wss://relay.example/ws/00000000-0000-0000-0000-000000000001/client","token":"pair-token","server_id":"00000000-0000-0000-0000-000000000001","expires_at_ms":1710000060000}"#;
    let invite = format!("termd-pair:v1:{}", URL_SAFE_NO_PAD.encode(json));

    let decoded = URL_SAFE_NO_PAD
        .decode(invite.trim_start_matches("termd-pair:v1:"))
        .unwrap();

    assert_eq!(String::from_utf8(decoded).unwrap(), json);
}
