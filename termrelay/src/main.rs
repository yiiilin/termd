//! termrelay 的 HTTP/WebSocket 入口。
//!
//! relay 负责 trusted admission 和按 `server_id` 路由 WebSocket frame。
//! pairing/auth/session/control 的最终业务判断仍只在 daemon 内执行。

mod args;
mod router;
mod ws;

use std::net::SocketAddr;
use std::path::PathBuf;

use rustls::pki_types::pem::PemObject;
use thiserror::Error;
use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::args::{ArgsError, RelayCommand};
use crate::router::router;
use crate::ws::{RelayDaemonCredential, RelayState};

#[derive(Debug, Error)]
enum MainError {
    #[error(transparent)]
    Args(#[from] ArgsError),
    #[error("failed to bind relay HTTP listener at {addr}")]
    Bind {
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },
    #[error("relay HTTP server failed")]
    Serve(#[source] std::io::Error),
    #[error("failed to load relay TLS certificate chain")]
    TlsCertificate(#[source] std::io::Error),
    #[error("failed to load relay TLS private key")]
    TlsPrivateKey(#[source] std::io::Error),
    #[error("relay TLS private key is missing")]
    MissingTlsPrivateKey,
    #[error("relay TLS configuration is invalid")]
    TlsConfig,
    #[error(
        "daemon registry is required; pass --daemon-registry or explicit --allow-open-relay for legacy/open relay mode"
    )]
    MissingDaemonRegistry,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TlsPaths {
    cert_path: PathBuf,
    key_path: PathBuf,
}

#[tokio::main]
async fn main() -> Result<(), MainError> {
    init_tracing();

    let args = match RelayCommand::from_env()? {
        RelayCommand::Help => {
            println!("{}", help_text());
            return Ok(());
        }
        RelayCommand::Version => {
            println!("termrelay {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        RelayCommand::Serve(args) => args,
    };
    let listener = TcpListener::bind(args.listen)
        .await
        .map_err(|source| MainError::Bind {
            addr: args.listen,
            source,
        })?;

    let tls = match (args.tls_cert, args.tls_key) {
        (Some(cert_path), Some(key_path)) => Some(TlsPaths {
            cert_path,
            key_path,
        }),
        _ => None,
    };

    info!(
        listen = %args.listen,
        tls = tls.is_some(),
        web = args.web,
        http_tunnel = args.http_tunnel,
        allow_open_relay = args.allow_open_relay,
        "starting trusted termrelay"
    );

    let daemon_credentials = args
        .daemon_registry
        .daemons
        .into_iter()
        .map(|daemon| RelayDaemonCredential {
            server_id: daemon.server_id,
            token: daemon.token,
        })
        .collect::<Vec<_>>();
    let state = if daemon_credentials.is_empty() {
        if !args.allow_open_relay {
            return Err(MainError::MissingDaemonRegistry);
        }
        RelayState::new(args.auth_token)
    } else {
        RelayState::new_trusted(args.auth_token, daemon_credentials)
    };

    serve_listener(listener, state, tls, args.web, args.http_tunnel).await
}

fn help_text() -> String {
    format!(
        concat!(
            "termrelay {}\n\n",
            "USAGE:\n",
            "  termrelay [OPTIONS]\n\n",
            "OPTIONS:\n",
            "  --listen, -l <HOST:PORT>      Listen address, default 127.0.0.1:8080\n",
            "  --auth-token <TOKEN>          Transport auth token required from daemon/client relay sockets\n",
            "  --auth-token-file <PATH>      Read transport auth token from a file; conflicts with --auth-token\n",
            "  --daemon-registry <PATH>      JSON daemon registry enabling trusted relay admission\n",
            "  --allow-open-relay            Explicitly allow legacy/open relay mode without daemon registry\n",
            "  --tls-cert <CERT_PEM>         TLS certificate path\n",
            "  --tls-key <KEY_PEM>           TLS private key path; must be paired with --tls-cert\n",
            "  --web                         Serve embedded Web UI\n",
            "  --http-tunnel                 Enable compatibility HTTP file tunnel paths\n",
            "  -h, --help                    Print help\n",
            "  -V, --version                 Print version\n\n",
            "EXAMPLES:\n",
            "  termrelay --listen 127.0.0.1:8080 --allow-open-relay\n",
            "  termrelay --listen 0.0.0.0:8080 --auth-token-file /run/secrets/termrelay_auth_token --daemon-registry /run/secrets/termrelay_daemons\n"
        ),
        env!("CARGO_PKG_VERSION")
    )
}

fn init_tracing() {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("termrelay=info"));

    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(false)
        .init();
}

async fn shutdown_signal() {
    // 监听 Ctrl-C 即可满足 MVP；systemd/k8s 等更复杂生命周期不进入本轮 relay。
    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::warn!(%error, "failed to listen for shutdown signal");
    }
}

async fn serve_listener(
    listener: TcpListener,
    state: RelayState,
    tls: Option<TlsPaths>,
    web_enabled: bool,
    http_tunnel_enabled: bool,
) -> Result<(), MainError> {
    match tls {
        Some(paths) => {
            serve_tls_listener(listener, state, paths, web_enabled, http_tunnel_enabled).await
        }
        None => axum::serve(listener, router(state, web_enabled, http_tunnel_enabled))
            .with_graceful_shutdown(shutdown_signal())
            .await
            .map_err(MainError::Serve),
    }
}

async fn serve_tls_listener(
    listener: TcpListener,
    state: RelayState,
    tls_paths: TlsPaths,
    web_enabled: bool,
    http_tunnel_enabled: bool,
) -> Result<(), MainError> {
    let tls_config = load_rustls_server_config(&tls_paths)?;
    serve_rustls_listener(
        listener,
        router(state, web_enabled, http_tunnel_enabled),
        tls_config,
    )
    .await
}

fn load_rustls_server_config(tls_paths: &TlsPaths) -> Result<rustls::ServerConfig, MainError> {
    let certs = rustls::pki_types::CertificateDer::pem_file_iter(&tls_paths.cert_path)
        .map_err(io_error_for_tls_cert)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(io_error_for_tls_cert)?;
    let key = rustls::pki_types::PrivateKeyDer::from_pem_file(&tls_paths.key_path)
        .map_err(io_error_for_tls_key)?;

    rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|_| MainError::TlsConfig)
}

fn io_error_for_tls_cert(error: rustls::pki_types::pem::Error) -> MainError {
    MainError::TlsCertificate(std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

fn io_error_for_tls_key(error: rustls::pki_types::pem::Error) -> MainError {
    match error {
        rustls::pki_types::pem::Error::NoItemsFound => MainError::MissingTlsPrivateKey,
        other => {
            MainError::TlsPrivateKey(std::io::Error::new(std::io::ErrorKind::InvalidData, other))
        }
    }
}

async fn serve_rustls_listener(
    listener: TcpListener,
    router: axum::Router,
    tls_config: rustls::ServerConfig,
) -> Result<(), MainError> {
    use axum_core::{body::Body, extract::Request};
    use hyper::body::Incoming;
    use hyper_util::rt::{TokioExecutor, TokioIo};
    use hyper_util::{server::conn::auto::Builder, service::TowerToHyperService};
    use std::sync::Arc;
    use tokio_rustls::TlsAcceptor;
    use tower::ServiceExt as _;

    let acceptor = TlsAcceptor::from(Arc::new(tls_config));

    loop {
        tokio::select! {
            _ = shutdown_signal() => {
                return Ok(());
            }
            accept = listener.accept() => {
                let (tcp_stream, _remote_addr) = accept.map_err(MainError::Serve)?;
                let acceptor = acceptor.clone();
                let service = router
                    .clone()
                    .map_request(|req: Request<Incoming>| req.map(Body::new));

                tokio::spawn(async move {
                    let tls_stream = match acceptor.accept(tcp_stream).await {
                        Ok(stream) => stream,
                        Err(error) => {
                            tracing::warn!(%error, "relay TLS handshake failed");
                            return;
                        }
                    };
                    let io = TokioIo::new(tls_stream);
                    let hyper_service = TowerToHyperService::new(service);
                    if let Err(error) = Builder::new(TokioExecutor::new())
                        .serve_connection_with_upgrades(io, hyper_service)
                        .await
                    {
                        tracing::warn!(%error, "relay TLS HTTP/WebSocket connection failed");
                    }
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test(flavor = "multi_thread")]
    async fn tls_listener_serves_healthz() {
        let (cert_path, key_path) = write_test_tls_files("healthz");
        let tls_paths = TlsPaths {
            cert_path: cert_path.clone(),
            key_path: key_path.clone(),
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ =
                serve_tls_listener(listener, RelayState::default(), tls_paths, false, false).await;
        });

        let response = tls_healthz_request(addr, &cert_path).await;
        server.abort();
        fs::remove_file(cert_path).ok();
        fs::remove_file(key_path).ok();

        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("\"status\":\"ok\""));
        assert!(response.contains("\"rooms\":0"));
    }

    #[test]
    fn tls_config_errors_do_not_leak_private_key_content() {
        let (cert_path, key_path) = write_test_tls_files("invalid-key");
        fs::write(&key_path, "not a private key\n").unwrap();
        let tls_paths = TlsPaths {
            cert_path: cert_path.clone(),
            key_path: key_path.clone(),
        };

        let error = load_rustls_server_config(&tls_paths).unwrap_err();
        let rendered_error = error.to_string();
        let rendered_paths = format!("{tls_paths:?}");

        assert!(matches!(
            error,
            MainError::MissingTlsPrivateKey | MainError::TlsPrivateKey(_)
        ));
        assert!(!rendered_error.contains("not a private key"));
        assert!(!rendered_paths.contains("termd-test-tls-invalid-key-key"));
        fs::remove_file(cert_path).ok();
        fs::remove_file(key_path).ok();
    }

    async fn tls_healthz_request(addr: SocketAddr, cert_path: &PathBuf) -> String {
        let mut root_store = rustls::RootCertStore::empty();
        let certs = rustls::pki_types::CertificateDer::pem_file_iter(cert_path)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        for cert in certs {
            root_store.add(cert).unwrap();
        }
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth();
        let connector = tokio_rustls::TlsConnector::from(std::sync::Arc::new(config));
        let server_name = rustls::pki_types::ServerName::try_from("localhost")
            .unwrap()
            .to_owned();
        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut stream = connector.connect(server_name, tcp).await.unwrap();
        let request = format!(
            "GET /healthz HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\n\r\n",
            port = addr.port()
        );
        stream.write_all(request.as_bytes()).await.unwrap();

        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        String::from_utf8(response).unwrap()
    }

    fn write_test_tls_files(name: &str) -> (PathBuf, PathBuf) {
        let stamp = format!("{}-{}", std::process::id(), uuid::Uuid::new_v4());
        let cert_path =
            std::env::temp_dir().join(format!("termd-test-tls-{name}-{stamp}-cert.pem"));
        let key_path = std::env::temp_dir().join(format!("termd-test-tls-{name}-{stamp}-key.pem"));
        fs::write(&cert_path, TEST_TLS_CERT_PEM).unwrap();
        fs::write(&key_path, TEST_TLS_KEY_PEM).unwrap();
        (cert_path, key_path)
    }

    const TEST_TLS_CERT_PEM: &str = r#"-----BEGIN CERTIFICATE-----
MIIDHzCCAgegAwIBAgIUFT0JPphPVviedOwVfBgtvRlWaBswDQYJKoZIhvcNAQEL
BQAwFDESMBAGA1UEAwwJbG9jYWxob3N0MB4XDTI2MDUwNzAzNDYxM1oXDTM2MDUw
NDAzNDYxM1owFDESMBAGA1UEAwwJbG9jYWxob3N0MIIBIjANBgkqhkiG9w0BAQEF
AAOCAQ8AMIIBCgKCAQEAp1LIkvOYe7VEamUgwSGpS3K9bH7DTl7sZXZLK4H4S3Ik
/68PSKWs8k+J079wrdq7Pft2u+NMACqwWK4uO30NetgQPGLB+awxqgLXyxyouTNp
XSX30gkxG1WhRWLq0JTtHZM86cFH3wZkrNIM6vzCGh5F/azICCkMyfoUJOkNezk2
T3nagv4/BeT/IDVNMEjRstwDGuuyOcKnvzUGtgwvvYbXuHmn956vAc7As3jAQNm1
eTFcg4FHzwDT5ZCYbeXeHGVtF+t+MXpbU9fbYncwLQNznni3Ngvg39XsEpsh17/I
shjHxjyJPs8Wx/TerRJ/frLcxvdFse044YcMZIQ9zQIDAQABo2kwZzAdBgNVHQ4E
FgQUVgawzOdJe6rn6Qc8o7sGNCOSJZcwHwYDVR0jBBgwFoAUVgawzOdJe6rn6Qc8
o7sGNCOSJZcwGgYDVR0RBBMwEYIJbG9jYWxob3N0hwR/AAABMAkGA1UdEwQCMAAw
DQYJKoZIhvcNAQELBQADggEBAEm25sfAoFRwcXTGJOfhEo9GM6JDESMxulolgR+4
IiwniOYUXvK5e51mszNzxu4AsG9OO4+myqEE0AXrhgG7kjFvUWwOVQ4wgwCUUfbj
qRpnH5SRYaKqQMJviz7adU0biGyRBN7+6YChZW8XEEE7+lGpDw979URChb/shtX7
Yb9UYaOsqvLRh+MHXMfZMPTawI1o5x6oar1a6D3SswB9omWPQABuFXeJeZcK4B/0
PEx176/dWuU6shATtBw9s3r4pJTJ5H+9awx7xyS9WYiVyt9SRxppJiwAPU9mS1Sa
T+luYJ3JUrIbrKq4qET6e3ut8nJZcnJbryvWVpegnuNiH6k=
-----END CERTIFICATE-----"#;

    const TEST_TLS_KEY_PEM: &str = r#"-----BEGIN PRIVATE KEY-----
MIIEvAIBADANBgkqhkiG9w0BAQEFAASCBKYwggSiAgEAAoIBAQCnUsiS85h7tURq
ZSDBIalLcr1sfsNOXuxldksrgfhLciT/rw9IpazyT4nTv3Ct2rs9+3a740wAKrBY
ri47fQ162BA8YsH5rDGqAtfLHKi5M2ldJffSCTEbVaFFYurQlO0dkzzpwUffBmSs
0gzq/MIaHkX9rMgIKQzJ+hQk6Q17OTZPedqC/j8F5P8gNU0wSNGy3AMa67I5wqe/
NQa2DC+9hte4eaf3nq8BzsCzeMBA2bV5MVyDgUfPANPlkJht5d4cZW0X634xeltT
19tidzAtA3OeeLc2C+Df1ewSmyHXv8iyGMfGPIk+zxbH9N6tEn9+stzG90Wx7Tjh
hwxkhD3NAgMBAAECggEABMD/Xd156Zne1b8FzTbtnm0mIJ0BY4qi4McZn6TTryER
GAqbPo8meMP1wIRh6S6bv0kTuIbes+qClCJuwdXtuh3FaFHN/Q/9YT0vcF/iE1D4
n2LixZ7pPEOUj2oeDcsNaZezVVjed+GwnpBhOZPw19kgV/K+xCyWZm6qf9n3Phb4
Pg9ODsq3+45cjk10Qvk+VWva1xcw8qHOpHbTLguZ3e13rL9HXbaZAfFvKGpDhzpX
m7dZ7jOqnpZt9oll8Ean2SIOfhQdACcsuz+FDIYVj1PufA3WlOeGq4gAfoBKGUNb
OFp49W0MHhSH/kmwhz9lF83okXqYJtZtxXGMiQOhKQKBgQDf4E2/BbcePEhdnMkq
wTygBN+eEyZcN5nPnNZZ8wefaLSoO3BMbkjyjr0kPQnN/FCFMWr2Rs0ga3kCN/rr
985D+DwObOSXtYBa16+w0bHoKOrxs27tX1Vnaj2djeTZggK/2k5l5YTcxrL+dSQI
LnYowViOacuaxcqy0nzRxQamowKBgQC/VRyxVh/5tB3aV2zhwZuM4RrhdpSpExql
Ohc7FAcM9X8ywjLc6ZSbGnd5j894P+EQpoJBLVxTExgasCWxuwdck4nv1dboGPZO
PodEIcz4FGOZ177oiJsJH/xkuNlliyh7i/Cyu97IXIXzFupMVEaAGIGTd2h8zhU9
wiQUUwaAzwKBgG8P14HsU+ur/Dp0jVeohWrdABJrbZxR+PwF0lDNP/rU9sp+sjc4
fvfV1/8iSLrncQqieW2zsg9jQaTYIKLvTGRrwV9mpgCdChAG8CHH5XpG0kcVvPIF
WVj0W5zNx7ofxT1oD3x9YGwmJqYVdsqYQgX15PjBg0BE30nXIhTuqV4BAoGAcWdF
BmcBtMLpHszKoFRcmfeiMxhRrJTCKkRwGHgaZbfsmG06MG3RwszBG6/9TEywXWoT
sgXsvuCGXOsirGEqT9iy3RBlvFNvSZkOG3fdQPz0u+6AHNs66QGoWxqk3+bHK9MZ
6xYnSaJtUlO2s18QGkRsKLeRmsebF2vGbrV3GUkCgYAT5lgVHUx435Zy9mOgWCEl
4OLdzEEZm8OmMiRDzgxHs0Nx4zCUYZRf5HaHUhz936R8Ez0DVCj1GAdQjkV1kCEI
joi6qSEnJBpLL35fFZfHkF1jBOfv8otRgWJuJwyit3B7LR89GAw2VgZWu03QugPN
zZZR5LzKVu9X7paftR7K8Q==
-----END PRIVATE KEY-----"#;
}
