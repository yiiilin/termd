import '../errors/native_error.dart';
import 'protocol_types.dart';

final class ProtocolConnectionStatus {
  const ProtocolConnectionStatus({
    required this.connected,
    required this.safeMessage,
  });

  const ProtocolConnectionStatus.unavailable()
      : connected = false,
        safeMessage = 'direct WebSocket/E2EE client 未接入';

  final bool connected;
  final String safeMessage;
}

final class TerminalAttachPreview {
  const TerminalAttachPreview({
    required this.sessionId,
    required this.safeMessage,
  });

  final String sessionId;
  final String safeMessage;
}

abstract interface class TermdProtocolClient {
  Future<ProtocolConnectionStatus> connectionStatus();

  Future<List<RemoteSessionSummary>> listSessions();

  Future<TerminalAttachPreview> prepareAttach(String sessionId);
}

final class UnsupportedTermdProtocolClient implements TermdProtocolClient {
  const UnsupportedTermdProtocolClient();

  @override
  Future<ProtocolConnectionStatus> connectionStatus() async {
    return const ProtocolConnectionStatus.unavailable();
  }

  @override
  Future<List<RemoteSessionSummary>> listSessions() async {
    // 协议 client 未接入时不返回伪会话，避免 UI 误以为已完成 daemon 通信。
    throw const ProtocolClientUnavailable();
  }

  @override
  Future<TerminalAttachPreview> prepareAttach(String sessionId) async {
    throw const ProtocolClientUnavailable();
  }
}
