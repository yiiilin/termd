import 'package:flutter_test/flutter_test.dart';
import 'package:termui_native/core/errors/native_error.dart';
import 'package:termui_native/core/protocol/protocol_client.dart';

void main() {
  test('unsupported protocol client fails closed instead of faking daemon state', () async {
    const client = UnsupportedTermdProtocolClient();

    final status = await client.connectionStatus();
    expect(status.connected, isFalse);
    expect(status.safeMessage, contains('未接入'));

    // Native 当前只保留协议边界模型；未接入 direct WebSocket/E2EE 前不能伪造会话或 attach。
    await expectLater(
      client.listSessions(),
      throwsA(isA<ProtocolClientUnavailable>()),
    );
    await expectLater(
      client.prepareAttach('session-test-1'),
      throwsA(isA<ProtocolClientUnavailable>()),
    );
  });
}
