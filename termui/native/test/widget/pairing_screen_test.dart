import 'dart:typed_data';

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:termui_native/core/device/device_identity.dart';
import 'package:termui_native/core/device/device_key_manager.dart';
import 'package:termui_native/core/device/paired_server.dart';
import 'package:termui_native/core/protocol/protocol_client.dart';
import 'package:termui_native/core/services/termui_native_service.dart';
import 'package:termui_native/features/pairing/pairing_screen.dart';

import '../support/fake_secure_storage.dart';

void main() {
  testWidgets('pairing screen only shows sanitized URL in placeholder status', (
    tester,
  ) async {
    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(body: PairingScreen(service: _serviceWithStorage())),
      ),
    );

    await tester.enterText(
      find.byType(TextField).first,
      'wss://Example.COM/ws?relay_token=relay-secret#pair-fragment',
    );
    await tester.enterText(
      find.byType(TextField).last,
      'PAIR-CODE-SHOULD-STAY-IN-MEMORY',
    );
    await tester.tap(find.text('准备配对'));
    await tester.pumpAndSettle();

    expect(find.textContaining('尚未接入真实配对'), findsOneWidget);
    expect(find.textContaining('wss://example.com/ws'), findsOneWidget);
    expect(find.textContaining('relay_token'), findsNothing);
    expect(find.textContaining('pair-fragment'), findsNothing);
  });
}

TermuiNativeService _serviceWithStorage() {
  final storage = FakeSecureStorage();
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
