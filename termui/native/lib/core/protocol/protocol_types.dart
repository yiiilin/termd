typedef JsonMap = Map<String, Object?>;

const ed25519WirePrefix = 'ed25519-v1:';
const x25519WirePrefix = 'x25519-v1:';

/// Dart 端这里只声明 Native 当前消费的低风险协议子集；wire 名称必须与 Rust 对齐，
/// 但这里不声称完整覆盖 Rust proto。
enum ProtocolMessageType {
  hello('hello'),
  auth('auth'),
  authChallenge('auth_challenge'),
  pairRequest('pair_request'),
  pairAccept('pair_accept'),
  sessionCreate('session_create'),
  sessionCreated('session_created'),
  sessionAttach('session_attach'),
  sessionAttached('session_attached'),
  sessionData('session_data'),
  sessionResize('session_resize'),
  sessionList('session_list'),
  sessionListResult('session_list_result'),
  controlRequest('control_request'),
  controlGrant('control_grant'),
  e2eeKeyExchange('e2ee_key_exchange'),
  encryptedFrame('encrypted_frame'),
  error('error'),
  ping('ping'),
  pong('pong');

  const ProtocolMessageType(this.wireName);

  final String wireName;
}

final class JsonEnvelope {
  const JsonEnvelope({
    required this.type,
    required this.payload,
  });

  final ProtocolMessageType type;
  final JsonMap payload;

  JsonMap toJson() => {
        'type': type.wireName,
        'payload': payload,
      };
}

enum SessionState {
  created('created'),
  running('running'),
  closed('closed');

  const SessionState(this.wireName);

  final String wireName;
}

enum AttachRole {
  sharedControlOperator('operator');

  const AttachRole(this.wireName);

  final String wireName;
}

final class TerminalSize {
  const TerminalSize({
    required this.rows,
    required this.cols,
    required this.pixelWidth,
    required this.pixelHeight,
  });

  const TerminalSize.defaultSize()
      : rows = 24,
        cols = 80,
        pixelWidth = 0,
        pixelHeight = 0;

  final int rows;
  final int cols;
  final int pixelWidth;
  final int pixelHeight;

  JsonMap toJson() => {
        'rows': rows,
        'cols': cols,
        'pixel_width': pixelWidth,
        'pixel_height': pixelHeight,
      };
}

final class EncryptedFramePayload {
  const EncryptedFramePayload({
    required this.serverId,
    required this.sequence,
    required this.ciphertextBase64,
  });

  final String serverId;
  final int sequence;
  final String ciphertextBase64;

  JsonMap toJson() => {
        'server_id': serverId,
        'sequence': sequence,
        'ciphertext_base64': ciphertextBase64,
      };
}

final class RemoteSessionSummary {
  const RemoteSessionSummary({
    required this.sessionId,
    this.name,
    required this.state,
    required this.size,
    this.filesPath,
    this.createdAtMs,
  });

  final String sessionId;
  final String? name;
  final SessionState state;
  final TerminalSize size;
  final String? filesPath;
  final int? createdAtMs;
}
