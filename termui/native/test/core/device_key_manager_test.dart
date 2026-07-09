import 'dart:convert';
import 'dart:typed_data';

import 'package:flutter_test/flutter_test.dart';
import 'package:termui_native/core/device/device_identity.dart';
import 'package:termui_native/core/device/device_key_manager.dart';
import 'package:termui_native/core/errors/native_error.dart';
import 'package:termui_native/core/storage/secure_storage_keys.dart';

import '../support/fake_secure_storage.dart';

void main() {
  test('loadOrCreateDevice stores identity as one versioned record', () async {
    final storage = FakeSecureStorage();
    final manager = DeviceKeyManager(
      storage: storage,
      generator: const FakeDeviceIdentityGenerator(),
      signer: FakeDeviceAuthSigner(),
    );

    final identity = await manager.loadOrCreateDevice();
    final raw = storage.values[SecureStorageKeys.deviceId]!;
    final decoded = jsonDecode(raw) as Map<String, dynamic>;

    expect(identity.deviceId, 'device-test-1');
    expect(decoded['version'], 1);
    expect(decoded['device_id'], 'device-test-1');
    expect(decoded['device_public_key'], 'ed25519-v1:device-public-1');
    expect(
      decoded['device_signing_key_secret'],
      'ed25519-v1:device-signing-secret-1',
    );
    expect(
      storage.values.containsKey(SecureStorageKeys.devicePublicKey),
      isFalse,
    );
    expect(
      storage.values.containsKey(SecureStorageKeys.deviceSigningKeySecret),
      isFalse,
    );
  });

  test('loadDevice returns only public identity', () async {
    final storage = FakeSecureStorage();
    _seedVersionedDeviceIdentity(storage);
    final manager = DeviceKeyManager(
      storage: storage,
      generator: const FakeDeviceIdentityGenerator(),
      signer: FakeDeviceAuthSigner(),
    );

    final identity = await manager.loadDevice();

    expect(identity?.deviceId, 'device-test-1');
    expect(identity?.devicePublicKey, 'ed25519-v1:device-public-1');
  });

  test('loadDevice migrates complete legacy identity keys to one record', () async {
    final storage = FakeSecureStorage()
      ..values[SecureStorageKeys.deviceId] = 'device-test-1'
      ..values[SecureStorageKeys.devicePublicKey] = 'ed25519-v1:device-public-1'
      ..values[SecureStorageKeys.deviceSigningKeySecret] =
          'ed25519-v1:device-signing-secret-1';
    final manager = DeviceKeyManager(
      storage: storage,
      generator: const FakeDeviceIdentityGenerator(),
      signer: FakeDeviceAuthSigner(),
    );

    final identity = await manager.loadDevice();
    final decoded = jsonDecode(storage.values[SecureStorageKeys.deviceId]!)
        as Map<String, dynamic>;

    expect(identity?.deviceId, 'device-test-1');
    expect(decoded['version'], 1);
    expect(
      storage.values.containsKey(SecureStorageKeys.devicePublicKey),
      isFalse,
    );
    expect(
      storage.values.containsKey(SecureStorageKeys.deviceSigningKeySecret),
      isFalse,
    );
  });

  test('partial device state is rejected', () async {
    final storage = FakeSecureStorage()
      ..values[SecureStorageKeys.deviceId] = 'device-test-1';
    final manager = DeviceKeyManager(
      storage: storage,
      generator: const FakeDeviceIdentityGenerator(),
      signer: FakeDeviceAuthSigner(),
    );

    expect(manager.loadDevice(), throwsA(isA<DeviceIdentityCorrupted>()));
  });

  test('corrupted versioned identity with malformed wire prefixes fails closed', () async {
    for (final rawRecord in <Map<String, Object?>>[
      <String, Object?>{
        'version': 1,
        'device_id': 'device-test-1',
        'device_public_key': 'device-public-key-without-prefix',
        'device_signing_key_secret': 'ed25519-v1:device-signing-secret-1',
      },
      <String, Object?>{
        'version': 1,
        'device_id': 'device-test-1',
        'device_public_key': 'ed25519-v1:device-public-1',
        'device_signing_key_secret': 'device-signing-secret-without-prefix',
      },
    ]) {
      final storage = FakeSecureStorage(<String, String>{
        SecureStorageKeys.deviceId: jsonEncode(rawRecord),
      });
      final manager = DeviceKeyManager(
        storage: storage,
        generator: const FakeDeviceIdentityGenerator(),
        signer: FakeDeviceAuthSigner(),
      );

      await expectLater(
        manager.loadDevice(),
        throwsA(isA<DeviceIdentityCorrupted>()),
      );
    }
  });

  test('corrupted legacy identity components with malformed wire prefixes fail closed', () async {
    final storage = FakeSecureStorage()
      ..values[SecureStorageKeys.deviceId] = 'device-test-1'
      ..values[SecureStorageKeys.devicePublicKey] = 'device-public-key-without-prefix'
      ..values[SecureStorageKeys.deviceSigningKeySecret] =
          'ed25519-v1:device-signing-secret-1';
    final manager = DeviceKeyManager(
      storage: storage,
      generator: const FakeDeviceIdentityGenerator(),
      signer: FakeDeviceAuthSigner(),
    );

    await expectLater(
      manager.loadDevice(),
      throwsA(isA<DeviceIdentityCorrupted>()),
    );
  });

  test('loadOrCreateDevice rejects generated malformed signing material', () async {
    final storage = FakeSecureStorage();
    final manager = DeviceKeyManager(
      storage: storage,
      generator: const _MalformedDeviceIdentityGenerator(),
      signer: FakeDeviceAuthSigner(),
    );

    await expectLater(
      manager.loadOrCreateDevice(),
      throwsA(isA<DeviceIdentityCorrupted>()),
    );
    expect(storage.values[SecureStorageKeys.deviceId], isNull);
  });

  test('signAuthInput does not return raw signing material', () async {
    final storage = FakeSecureStorage();
    _seedVersionedDeviceIdentity(storage);
    final signer = FakeDeviceAuthSigner();
    final manager = DeviceKeyManager(
      storage: storage,
      generator: const FakeDeviceIdentityGenerator(),
      signer: signer,
    );

    final signature = await manager.signAuthInput(Uint8List.fromList([1, 2, 3]));

    expect(signer.observedSecret, 'ed25519-v1:device-signing-secret-1');
    expect(signature.wireValue, 'ed25519-v1:test-signature-3');
    expect(signature.wireValue, isNot(contains('device-signing-secret')));
  });

  test('production unsupported path fails closed', () async {
    final manager = DeviceKeyManager.productionUnsupported();

    expect(manager.loadDevice(), throwsA(isA<SecureStorageUnavailable>()));
    expect(manager.loadOrCreateDevice(), throwsA(isA<SecureStorageUnavailable>()));
  });
}

void _seedVersionedDeviceIdentity(FakeSecureStorage storage) {
  storage.values[SecureStorageKeys.deviceId] = jsonEncode(<String, Object?>{
    'version': 1,
    'device_id': 'device-test-1',
    'device_public_key': 'ed25519-v1:device-public-1',
    'device_signing_key_secret': 'ed25519-v1:device-signing-secret-1',
  });
}

final class FakeDeviceIdentityGenerator implements DeviceIdentityGenerator {
  const FakeDeviceIdentityGenerator();

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

final class FakeDeviceAuthSigner implements DeviceAuthSigner {
  String? observedSecret;

  @override
  Future<DeviceSignature> sign({
    required String signingKeySecret,
    required Uint8List canonicalInput,
  }) async {
    observedSecret = signingKeySecret;
    return DeviceSignature('ed25519-v1:test-signature-${canonicalInput.length}');
  }
}

final class _MalformedDeviceIdentityGenerator implements DeviceIdentityGenerator {
  const _MalformedDeviceIdentityGenerator();

  @override
  Future<GeneratedDeviceKeyMaterial> generate() async {
    return const GeneratedDeviceKeyMaterial(
      publicIdentity: DevicePublicIdentity(
        deviceId: 'device-test-1',
        devicePublicKey: 'device-public-key-without-prefix',
      ),
      signingKeySecret: 'device-signing-secret-without-prefix',
    );
  }
}
