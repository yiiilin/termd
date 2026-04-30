//! daemon 主动连接 relay 的 outbound mux 适配层。
//!
//! relay 只负责把 client frame 包进 `RelayMuxEnvelope` 并按 `client_id` 转发；这里才把
//! 每个 relay client 映射成独立的 daemon `ProtocolConnection`。

use std::collections::HashMap;

use base64::{Engine as _, engine::general_purpose};
use futures_util::{SinkExt, StreamExt};
use termd_proto::{RelayClientId, RelayMuxEnvelope, RelayOpaqueFrame, ServerId};
use thiserror::Error;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, warn};

use super::protocol::{JsonEnvelope, ProtocolConnection, ProtocolError};
use super::server::SharedDaemonProtocol;

const OUTPUT_FLUSH_MAX_BYTES_PER_SESSION: usize = 16 * 1024;

#[derive(Debug, Error)]
pub enum RelayConnectorError {
    #[error("unsupported relay URL; expected ws://host:port")]
    UnsupportedUrl,
    #[error("failed to connect relay daemon mux websocket")]
    ConnectFailed,
    #[error("relay websocket receive failed")]
    ReceiveFailed,
    #[error("relay websocket send failed")]
    SendFailed,
    #[error("relay mux envelope is invalid")]
    InvalidEnvelope,
    #[error("relay mux frame is invalid")]
    InvalidFrame,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayBaseUrl {
    authority: String,
}

impl RelayBaseUrl {
    pub fn parse(value: &str) -> Result<Self, RelayConnectorError> {
        let Some(rest) = value.strip_prefix("ws://") else {
            return Err(RelayConnectorError::UnsupportedUrl);
        };
        if rest.is_empty() || rest.contains('?') || rest.contains('#') {
            return Err(RelayConnectorError::UnsupportedUrl);
        }

        let authority = match rest.split_once('/') {
            Some((authority, "")) => authority,
            Some(_) => return Err(RelayConnectorError::UnsupportedUrl),
            None => rest,
        };
        validate_authority(authority)?;
        Ok(Self {
            authority: authority.to_owned(),
        })
    }

    pub fn daemon_mux_url(&self, server_id: ServerId) -> String {
        format!("ws://{}/ws/{}/daemon-mux", self.authority, server_id.0)
    }
}

pub async fn connect_relay_mux(
    relay_url: &str,
    protocol: SharedDaemonProtocol,
) -> Result<(), RelayConnectorError> {
    let base = RelayBaseUrl::parse(relay_url)?;
    connect_relay_mux_base(base, protocol).await
}

pub async fn connect_relay_mux_base(
    base: RelayBaseUrl,
    protocol: SharedDaemonProtocol,
) -> Result<(), RelayConnectorError> {
    let server_id = {
        protocol
            .lock()
            .expect("daemon protocol mutex poisoned")
            .server_id()
    };
    let url = base.daemon_mux_url(server_id);
    let (socket, _) = connect_async(url)
        .await
        .map_err(|_| RelayConnectorError::ConnectFailed)?;
    let (mut sender, mut receiver) = socket.split();
    let mut connections = HashMap::<RelayClientId, ProtocolConnection>::new();

    while let Some(message) = receiver.next().await {
        let message = message.map_err(|_| RelayConnectorError::ReceiveFailed)?;
        match message {
            Message::Text(raw) => {
                let envelope: RelayMuxEnvelope = serde_json::from_str(raw.as_str())
                    .map_err(|_| RelayConnectorError::InvalidEnvelope)?;
                let responses = handle_mux_envelope(envelope, &protocol, &mut connections)?;
                send_mux_envelopes(&mut sender, responses).await?;
            }
            Message::Binary(raw) => {
                let envelope: RelayMuxEnvelope = serde_json::from_slice(&raw)
                    .map_err(|_| RelayConnectorError::InvalidEnvelope)?;
                let responses = handle_mux_envelope(envelope, &protocol, &mut connections)?;
                send_mux_envelopes(&mut sender, responses).await?;
            }
            Message::Ping(payload) => sender
                .send(Message::Pong(payload))
                .await
                .map_err(|_| RelayConnectorError::SendFailed)?,
            Message::Pong(_) => {}
            Message::Close(_) => break,
            Message::Frame(_) => {}
        }
    }

    close_relay_connections(protocol, connections);
    Ok(())
}

fn handle_mux_envelope(
    envelope: RelayMuxEnvelope,
    protocol: &SharedDaemonProtocol,
    connections: &mut HashMap<RelayClientId, ProtocolConnection>,
) -> Result<Vec<RelayMuxEnvelope>, RelayConnectorError> {
    match envelope {
        RelayMuxEnvelope::ClientConnected { client_id } => {
            let (connection, initial_messages) = {
                let protocol = protocol.lock().expect("daemon protocol mutex poisoned");
                protocol.start_connection()
            };
            connections.insert(client_id, connection);
            debug!(
                client_id = client_id.0,
                "relay client connected to daemon mux"
            );
            client_envelopes(client_id, initial_messages)
        }
        RelayMuxEnvelope::ClientDisconnected { client_id } => {
            if let Some(mut connection) = connections.remove(&client_id) {
                let mut protocol = protocol.lock().expect("daemon protocol mutex poisoned");
                connection.close(&mut protocol);
            }
            debug!(
                client_id = client_id.0,
                "relay client disconnected from daemon mux"
            );
            Ok(Vec::new())
        }
        RelayMuxEnvelope::ClientFrame { client_id, frame } => {
            if !connections.contains_key(&client_id) {
                warn!(
                    client_id = client_id.0,
                    "dropping relay frame for unknown client"
                );
                return Ok(Vec::new());
            };

            let frame = match json_envelope_from_mux_frame(frame) {
                Ok(frame) => frame,
                Err(error) => {
                    // relay client 是非可信输入源；坏业务 frame 只能影响该 client，不能杀掉
                    // daemon outbound connector 或 direct daemon。
                    close_client_connection(protocol, connections, client_id);
                    warn!(
                        client_id = client_id.0,
                        %error,
                        "closed relay client after invalid daemon protocol frame"
                    );
                    return Ok(Vec::new());
                }
            };
            let responses = {
                let connection = connections
                    .get_mut(&client_id)
                    .expect("connection existence checked before frame parsing");
                let mut protocol = protocol.lock().expect("daemon protocol mutex poisoned");
                let mut responses = connection.handle_wire_envelope(&mut protocol, frame);
                responses.extend(
                    connection
                        .read_attached_outputs(&mut protocol, OUTPUT_FLUSH_MAX_BYTES_PER_SESSION),
                );
                responses
            };
            client_envelopes(client_id, responses)
        }
        RelayMuxEnvelope::DaemonFrame { .. } => Err(RelayConnectorError::InvalidEnvelope),
    }
}

fn client_envelopes(
    client_id: RelayClientId,
    envelopes: Vec<JsonEnvelope>,
) -> Result<Vec<RelayMuxEnvelope>, RelayConnectorError> {
    envelopes
        .into_iter()
        .map(|envelope| {
            let raw = serde_json::to_string(&envelope)
                .map_err(|_| RelayConnectorError::InvalidEnvelope)?;
            Ok(RelayMuxEnvelope::DaemonFrame {
                client_id,
                frame: RelayOpaqueFrame::Text { data: raw },
            })
        })
        .collect()
}

async fn send_mux_envelopes(
    sender: &mut futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        Message,
    >,
    envelopes: Vec<RelayMuxEnvelope>,
) -> Result<(), RelayConnectorError> {
    for envelope in envelopes {
        let raw =
            serde_json::to_string(&envelope).map_err(|_| RelayConnectorError::InvalidEnvelope)?;
        sender
            .send(Message::Text(raw.into()))
            .await
            .map_err(|_| RelayConnectorError::SendFailed)?;
    }
    Ok(())
}

fn json_envelope_from_mux_frame(
    frame: RelayOpaqueFrame,
) -> Result<JsonEnvelope, RelayConnectorError> {
    match frame {
        RelayOpaqueFrame::Text { data } => {
            serde_json::from_str(&data).map_err(|_| RelayConnectorError::InvalidFrame)
        }
        RelayOpaqueFrame::Binary { data_base64 } => {
            let bytes = general_purpose::STANDARD
                .decode(data_base64)
                .map_err(|_| RelayConnectorError::InvalidFrame)?;
            serde_json::from_slice(&bytes).map_err(|_| RelayConnectorError::InvalidFrame)
        }
    }
}

fn validate_authority(authority: &str) -> Result<(), RelayConnectorError> {
    if authority.is_empty() || authority.contains('@') {
        return Err(RelayConnectorError::UnsupportedUrl);
    }
    if let Some(after_bracket) = authority.strip_prefix('[') {
        let Some((host, port)) = after_bracket.split_once("]:") else {
            return Err(RelayConnectorError::UnsupportedUrl);
        };
        if host.is_empty() || port.parse::<u16>().is_err() {
            return Err(RelayConnectorError::UnsupportedUrl);
        }
        return Ok(());
    }

    let Some((host, port)) = authority.rsplit_once(':') else {
        return Err(RelayConnectorError::UnsupportedUrl);
    };
    if host.is_empty() || port.parse::<u16>().is_err() {
        return Err(RelayConnectorError::UnsupportedUrl);
    }
    Ok(())
}

fn close_relay_connections(
    protocol: SharedDaemonProtocol,
    connections: HashMap<RelayClientId, ProtocolConnection>,
) {
    if connections.is_empty() {
        return;
    }

    let mut protocol = protocol.lock().expect("daemon protocol mutex poisoned");
    for (_client_id, mut connection) in connections {
        connection.close(&mut protocol);
    }
}

fn close_client_connection(
    protocol: &SharedDaemonProtocol,
    connections: &mut HashMap<RelayClientId, ProtocolConnection>,
    client_id: RelayClientId,
) {
    if let Some(mut connection) = connections.remove(&client_id) {
        let mut protocol = protocol.lock().expect("daemon protocol mutex poisoned");
        connection.close(&mut protocol);
    }
}

impl From<ProtocolError> for RelayConnectorError {
    fn from(_: ProtocolError) -> Self {
        Self::InvalidEnvelope
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::current_unix_timestamp_millis;
    use crate::config::DaemonConfig;
    use crate::net::protocol::{decode_payload, encrypted_frame_from_envelope, envelope_value};
    use crate::net::server::default_protocol;
    use crate::net::{E2eeKeyPair, E2eeSession, E2eeSessionContext, E2eeSessionRole};
    use termd_proto::{Envelope, MessageType, PingPayload};

    #[test]
    fn parses_relay_base_url_and_builds_daemon_mux_url() {
        let server_id = ServerId::new();
        let base = RelayBaseUrl::parse("ws://127.0.0.1:8080/").unwrap();
        let url = base.daemon_mux_url(server_id);

        assert_eq!(
            url,
            format!("ws://127.0.0.1:8080/ws/{}/daemon-mux", server_id.0)
        );
    }

    #[test]
    fn rejects_unsupported_relay_urls() {
        assert!(RelayBaseUrl::parse("http://127.0.0.1:8080").is_err());
        assert!(RelayBaseUrl::parse("ws://127.0.0.1:8080/path").is_err());
        assert!(RelayBaseUrl::parse("ws://127.0.0.1").is_err());
    }

    #[test]
    fn decodes_text_and_binary_mux_frames_as_json_envelopes() {
        let envelope = Envelope::new(
            MessageType::Ping,
            serde_json::to_value(PingPayload {
                nonce: termd_proto::Nonce("n".to_owned()),
                timestamp_ms: termd_proto::UnixTimestampMillis(1),
            })
            .unwrap(),
        );
        let raw = serde_json::to_string(&envelope).unwrap();
        let decoded_text =
            json_envelope_from_mux_frame(RelayOpaqueFrame::Text { data: raw.clone() }).unwrap();
        let decoded_binary = json_envelope_from_mux_frame(RelayOpaqueFrame::Binary {
            data_base64: general_purpose::STANDARD.encode(raw.as_bytes()),
        })
        .unwrap();

        assert_eq!(decoded_text.kind, MessageType::Ping);
        assert_eq!(decoded_binary.kind, MessageType::Ping);
    }

    #[test]
    fn mux_client_connection_can_complete_pairing_on_independent_protocol_connection() {
        let protocol = default_protocol(DaemonConfig::default());
        let client_id = RelayClientId(42);
        let mut connections = HashMap::new();

        let initial = handle_mux_envelope(
            RelayMuxEnvelope::ClientConnected { client_id },
            &protocol,
            &mut connections,
        )
        .unwrap();
        let hello = daemon_frame_to_json(initial[0].clone());
        let key_exchange = daemon_frame_to_json(initial[1].clone());
        assert_eq!(hello.kind, MessageType::Hello);
        assert_eq!(key_exchange.kind, MessageType::E2eeKeyExchange);

        let server_key_exchange: termd_proto::E2eeKeyExchangePayload =
            decode_payload(key_exchange.payload).unwrap();
        let token = protocol
            .lock()
            .expect("daemon protocol mutex poisoned")
            .issue_pairing_token(current_unix_timestamp_millis())
            .unwrap()
            .token()
            .0
            .clone();
        let device_id = termd_proto::DeviceId::new();
        let device_keypair = E2eeKeyPair::generate();
        let server_e2ee_key =
            crate::net::E2eePeerPublicKey::try_from(&server_key_exchange.public_key).unwrap();
        let context = E2eeSessionContext::new(
            server_key_exchange.server_id,
            device_id,
            server_e2ee_key,
            device_keypair.public_key(),
        );
        let mut device_e2ee = E2eeSession::new(
            E2eeSessionRole::Device,
            &device_keypair,
            server_e2ee_key,
            context,
        )
        .unwrap();

        let device_key_exchange = envelope_value(
            MessageType::E2eeKeyExchange,
            termd_proto::E2eeKeyExchangePayload {
                server_id: server_key_exchange.server_id,
                device_id,
                public_key: device_keypair.public_key_wire(),
                nonce: termd_proto::Nonce("relay-e2ee-nonce".to_owned()),
                timestamp_ms: current_unix_timestamp_millis(),
            },
        )
        .unwrap();
        let handshake_responses = handle_mux_envelope(
            RelayMuxEnvelope::ClientFrame {
                client_id,
                frame: json_to_mux_text(device_key_exchange),
            },
            &protocol,
            &mut connections,
        )
        .unwrap();
        assert!(handshake_responses.is_empty());

        let pair_request = envelope_value(
            MessageType::PairRequest,
            termd_proto::PairRequestPayload {
                device_id,
                device_public_key: termd_proto::PublicKey("ed25519-v1:test-device".to_owned()),
                token: termd_proto::PairingToken(token),
                nonce: termd_proto::Nonce("relay-pair-nonce".to_owned()),
                timestamp_ms: current_unix_timestamp_millis(),
            },
        )
        .unwrap();
        let encrypted = envelope_value(
            MessageType::EncryptedFrame,
            device_e2ee.encrypt_json_payload(&pair_request).unwrap(),
        )
        .unwrap();
        let pair_responses = handle_mux_envelope(
            RelayMuxEnvelope::ClientFrame {
                client_id,
                frame: json_to_mux_text(encrypted),
            },
            &protocol,
            &mut connections,
        )
        .unwrap();

        let outer = daemon_frame_to_json(pair_responses[0].clone());
        let frame = encrypted_frame_from_envelope(outer).unwrap();
        let inner: JsonEnvelope = device_e2ee.decrypt_json_payload(&frame).unwrap();
        let accepted: termd_proto::PairAcceptPayload = decode_payload(inner.payload).unwrap();

        assert_eq!(inner.kind, MessageType::PairAccept);
        assert_eq!(accepted.device_id, device_id);
        assert_eq!(accepted.server_id, server_key_exchange.server_id);
    }

    #[test]
    fn invalid_mux_client_frame_closes_only_that_client_connection() {
        let protocol = default_protocol(DaemonConfig::default());
        let bad_client_id = RelayClientId(1);
        let good_client_id = RelayClientId(2);
        let mut connections = HashMap::new();

        handle_mux_envelope(
            RelayMuxEnvelope::ClientConnected {
                client_id: bad_client_id,
            },
            &protocol,
            &mut connections,
        )
        .unwrap();
        let bad_result = handle_mux_envelope(
            RelayMuxEnvelope::ClientFrame {
                client_id: bad_client_id,
                frame: RelayOpaqueFrame::Text {
                    data: "not-json".to_owned(),
                },
            },
            &protocol,
            &mut connections,
        );

        assert!(bad_result.unwrap().is_empty());
        assert!(!connections.contains_key(&bad_client_id));

        let initial = handle_mux_envelope(
            RelayMuxEnvelope::ClientConnected {
                client_id: good_client_id,
            },
            &protocol,
            &mut connections,
        )
        .unwrap();
        assert_eq!(
            daemon_frame_to_json(initial[0].clone()).kind,
            MessageType::Hello
        );

        let key_exchange = daemon_frame_to_json(initial[1].clone());
        let server_key_exchange: termd_proto::E2eeKeyExchangePayload =
            decode_payload(key_exchange.payload).unwrap();
        let token = protocol
            .lock()
            .expect("daemon protocol mutex poisoned")
            .issue_pairing_token(current_unix_timestamp_millis())
            .unwrap()
            .token()
            .0
            .clone();
        let pair_response = complete_pairing_via_mux(
            &protocol,
            &mut connections,
            good_client_id,
            server_key_exchange,
            token,
        );

        assert_eq!(pair_response.kind, MessageType::PairAccept);
        assert!(connections.contains_key(&good_client_id));
    }

    fn complete_pairing_via_mux(
        protocol: &SharedDaemonProtocol,
        connections: &mut HashMap<RelayClientId, ProtocolConnection>,
        client_id: RelayClientId,
        server_key_exchange: termd_proto::E2eeKeyExchangePayload,
        token: String,
    ) -> JsonEnvelope {
        let device_id = termd_proto::DeviceId::new();
        let device_keypair = E2eeKeyPair::generate();
        let server_e2ee_key =
            crate::net::E2eePeerPublicKey::try_from(&server_key_exchange.public_key).unwrap();
        let context = E2eeSessionContext::new(
            server_key_exchange.server_id,
            device_id,
            server_e2ee_key,
            device_keypair.public_key(),
        );
        let mut device_e2ee = E2eeSession::new(
            E2eeSessionRole::Device,
            &device_keypair,
            server_e2ee_key,
            context,
        )
        .unwrap();
        let device_key_exchange = envelope_value(
            MessageType::E2eeKeyExchange,
            termd_proto::E2eeKeyExchangePayload {
                server_id: server_key_exchange.server_id,
                device_id,
                public_key: device_keypair.public_key_wire(),
                nonce: termd_proto::Nonce("relay-e2ee-nonce".to_owned()),
                timestamp_ms: current_unix_timestamp_millis(),
            },
        )
        .unwrap();
        let handshake_responses = handle_mux_envelope(
            RelayMuxEnvelope::ClientFrame {
                client_id,
                frame: json_to_mux_text(device_key_exchange),
            },
            protocol,
            connections,
        )
        .unwrap();
        assert!(handshake_responses.is_empty());

        let pair_request = envelope_value(
            MessageType::PairRequest,
            termd_proto::PairRequestPayload {
                device_id,
                device_public_key: termd_proto::PublicKey("ed25519-v1:test-device".to_owned()),
                token: termd_proto::PairingToken(token),
                nonce: termd_proto::Nonce("relay-pair-nonce".to_owned()),
                timestamp_ms: current_unix_timestamp_millis(),
            },
        )
        .unwrap();
        let encrypted = envelope_value(
            MessageType::EncryptedFrame,
            device_e2ee.encrypt_json_payload(&pair_request).unwrap(),
        )
        .unwrap();
        let pair_responses = handle_mux_envelope(
            RelayMuxEnvelope::ClientFrame {
                client_id,
                frame: json_to_mux_text(encrypted),
            },
            protocol,
            connections,
        )
        .unwrap();

        let outer = daemon_frame_to_json(pair_responses[0].clone());
        let frame = encrypted_frame_from_envelope(outer).unwrap();
        device_e2ee.decrypt_json_payload(&frame).unwrap()
    }

    fn json_to_mux_text(envelope: JsonEnvelope) -> RelayOpaqueFrame {
        RelayOpaqueFrame::Text {
            data: serde_json::to_string(&envelope).unwrap(),
        }
    }

    fn daemon_frame_to_json(envelope: RelayMuxEnvelope) -> JsonEnvelope {
        let RelayMuxEnvelope::DaemonFrame { frame, .. } = envelope else {
            panic!("expected daemon_frame");
        };
        json_envelope_from_mux_frame(frame).unwrap()
    }
}
