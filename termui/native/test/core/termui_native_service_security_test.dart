import 'package:flutter_test/flutter_test.dart';
import 'package:termui_native/core/errors/native_error.dart';
import 'package:termui_native/core/services/termui_native_service.dart';

void main() {
  test('production unsupported service cannot persist pairing material', () async {
    final service = TermuiNativeService.productionUnsupported();

    final snapshot = await service.loadWorkbenchSnapshot();
    expect(snapshot.device, isNull);
    expect(snapshot.pairedServers, isEmpty);
    expect(snapshot.statusMessage, contains('安全存储'));

    // 即使传入一次性配对码，production unsupported 也必须停在 secure storage 边界。
    await expectLater(
      service.preparePairing(
        url: 'ws://127.0.0.1:8765/ws',
        oneTimePairingCode: 'PAIRING-TOKEN-SHOULD-NOT-PERSIST',
      ),
      throwsA(isA<SecureStorageUnavailable>()),
    );
  });
}
