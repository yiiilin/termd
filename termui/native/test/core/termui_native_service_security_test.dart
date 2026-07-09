import 'dart:typed_data';

import 'package:flutter_test/flutter_test.dart';
import 'package:termui_native/core/device/device_identity.dart';
import 'package:termui_native/core/device/device_key_manager.dart';
import 'package:termui_native/core/device/paired_server.dart';
import 'package:termui_native/core/errors/native_error.dart';
import 'package:termui_native/core/protocol/protocol_client.dart';
import 'package:termui_native/core/services/termui_native_service.dart';
import 'package:termui_native/core/storage/secure_storage_keys.dart';

import '../support/fake_secure_storage.dart';

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

  test('preparePairing returns a sanitized URL and explicit placeholder status', () async {
    final storage = FakeSecureStorage();
    final service = _serviceWithStorage(storage);

    final preparation = await service.preparePairing(
      url:
          '  WSS://Example.COM:443/shell/../ws?relay_token=relay-secret#pair-fragment  ',
      oneTimePairingCode: 'PAIR-CODE-SHOULD-STAY-IN-MEMORY',
    );

    expect(preparation.url, 'wss://example.com:443/ws');
    expect(preparation.statusMessage, contains('尚未接入真实配对'));
    expect(preparation.statusMessage, contains('wss://example.com:443/ws'));
    expect(preparation.statusMessage, isNot(contains('relay_token')));
    expect(preparation.statusMessage, isNot(contains('pair-fragment')));
    expect(
      storage.values.values.join('\n'),
      isNot(contains('PAIR-CODE-SHOULD-STAY-IN-MEMORY')),
    );
    expect(storage.values[SecureStorageKeys.deviceId], isNotNull);
  });

  test('preparePairing rejects non-websocket or hostless URLs', () async {
    final service = _serviceWithStorage(FakeSecureStorage());

    for (final url in <String>[
      'https://example.test/ws',
      'ws:///missing-host',
      'not a url',
      '',
    ]) {
      await expectLater(
        service.preparePairing(
          url: url,
          oneTimePairingCode: 'PAIR-CODE-SHOULD-STAY-IN-MEMORY',
        ),
        throwsA(isA<NativeValidationError>()),
      );
    }
  });

  test('preparePairing rejects blank one-time pairing codes', () async {
    final service = _serviceWithStorage(FakeSecureStorage());

    await expectLater(
      service.preparePairing(
        url: 'ws://127.0.0.1:8765/ws',
        oneTimePairingCode: '   ',
      ),
      throwsA(isA<NativeValidationError>()),
    );
  });
}

TermuiNativeService _serviceWithStorage(FakeSecureStorage storage) {
  final keyManager = DeviceKeyManager(
    storage: storage,
    generator: const _FakeDeviceIdentityGenerator(),
    signer: _FakeDeviceAuthSigner(),
  );

  return TermuiNativeService(
    deviceKeyManager: keyManager,
    pairedServerStore: PairedServerStore(storage),
    protocolClient: const UnsupportedTermdProtocolClient(),
  );
}

final class _FakeDeviceIdentityGenerator implements DeviceIdentityGenerator {
  const _FakeDeviceIdentityGenerator();

  @override
  Future<GeneratedDeviceKeyMaterial> generate() async {
    return const GeneratedDeviceKeyMaterial(
      publicIdentity: DevicePublicIdentity(
        deviceId: 'device-test-1',
        devicePublicKey: 'ed25519-v1:device-public-1',
      ),
      signingKeySecret: 'ed25519-v1:device-signing-secret-1',
    );
  }
}

final class _FakeDeviceAuthSigner implements DeviceAuthSigner {
  @override
  Future<DeviceSignature> sign({
    required String signingKeySecret,
    required Uint8List canonicalInput,
  }) async {
    return const DeviceSignature('ed25519-v1:test-signature');
  }
}
