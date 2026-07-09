import 'package:flutter_test/flutter_test.dart';
import 'package:termui_native/core/protocol/protocol_types.dart';

void main() {
  test('attach role stays aligned with Rust shared-control operator model', () {
    expect(AttachRole.sharedControlOperator.wireName, 'operator');
  });

  test('remote session summary carries the low-risk Rust metadata fields', () {
    const summary = RemoteSessionSummary(
      sessionId: 'session-test-1',
      name: 'work shell',
      state: SessionState.running,
      size: TerminalSize.defaultSize(),
      filesPath: '/home/me/project',
      createdAtMs: 1710000000000,
    );

    expect(summary.name, 'work shell');
    expect(summary.filesPath, '/home/me/project');
    expect(summary.createdAtMs, 1710000000000);
  });
}
