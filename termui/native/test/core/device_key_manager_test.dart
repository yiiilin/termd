import 'dart:typed_data';

import 'package:flutter_test/flutter_test.dart';
import 'package:termui_native/core/device/device_identity.dart';
import 'package:termui_native/core/device/device_key_manager.dart';
import 'package:termui_native/core/errors/native_error.dart';
import 'package:termui_native/core/storage/secure_storage.dart';
import 'package:termui_native/core/storage/secure_storage_keys.dart';

void main() {
  test('loadOrCreateDevice stores secret only behind secure storage keys', () async {
    final storage = FakeSecureStorage();
    final manager = DeviceKeyManager(
      storage: storage,
      generator: const FakeDeviceIdentityGenerator(),
      signer: FakeDeviceAuthSigner(),
    );

    final identity = await manager.loadOrCreateDevice();

    expect(identity.deviceId, 'device-test-1');
    expect(storage.values[SecureStorageKeys.deviceId], 'device-test-1');
    expect(
      storage.values[SecureStorageKeys.devicePublicKey],
      'ed25519-v1:device-public-1',
    );
    expect(
      storage.values[SecureStorageKeys.deviceSigningKeySecret],
      'ed25519-v1:device-signing-secret-1',
    );
  });

  test('loadDevice returns only public identity', () async {
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

    expect(identity?.deviceId, 'device-test-1');
    expect(identity?.devicePublicKey, 'ed25519-v1:device-public-1');
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

  test('signAuthInput does not return raw signing material', () async {
    final storage = FakeSecureStorage()
      ..values[SecureStorageKeys.deviceSigningKeySecret] =
          'ed25519-v1:device-signing-secret-1';
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

final class FakeSecureStorage implements SecureStorage {
  final Map<String, String> values = <String, String>{};

  @override
  Future<void> delete(String key) async {
    values.remove(key);
  }

  @override
  Future<String?> read(String key) async {
    return values[key];
  }

  @override
  Future<void> write(String key, String value) async {
    values[key] = value;
  }
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
