import 'dart:typed_data';

import '../errors/native_error.dart';
import '../storage/secure_storage.dart';
import '../storage/secure_storage_keys.dart';
import 'device_identity.dart';

abstract interface class DeviceIdentityGenerator {
  Future<GeneratedDeviceKeyMaterial> generate();
}

abstract interface class DeviceAuthSigner {
  Future<DeviceSignature> sign({
    required String signingKeySecret,
    required Uint8List canonicalInput,
  });
}

final class GeneratedDeviceKeyMaterial {
  const GeneratedDeviceKeyMaterial({
    required this.publicIdentity,
    required this.signingKeySecret,
  });

  final DevicePublicIdentity publicIdentity;

  /// 只允许 DeviceKeyManager 写入 secure storage；UI 不得读取或展示该字段。
  final String signingKeySecret;
}

final class UnsupportedDeviceIdentityGenerator
    implements DeviceIdentityGenerator {
  const UnsupportedDeviceIdentityGenerator();

  @override
  Future<GeneratedDeviceKeyMaterial> generate() async {
    // 当前 item 不伪造真实 Ed25519 设备身份，避免后续与 Rust wire 兼容发生漂移。
    throw const SecureStorageUnavailable();
  }
}

final class UnsupportedDeviceAuthSigner implements DeviceAuthSigner {
  const UnsupportedDeviceAuthSigner();

  @override
  Future<DeviceSignature> sign({
    required String signingKeySecret,
    required Uint8List canonicalInput,
  }) async {
    // 签名能力必须等真实 Ed25519 adapter 接入后才可用；不能返回占位签名冒充成功。
    throw const DeviceSigningUnsupported();
  }
}

final class DeviceKeyManager {
  const DeviceKeyManager({
    required SecureStorage storage,
    required DeviceIdentityGenerator generator,
    required DeviceAuthSigner signer,
  })  : _storage = storage,
        _generator = generator,
        _signer = signer;

  factory DeviceKeyManager.productionUnsupported() {
    return const DeviceKeyManager(
      storage: UnsupportedSecureStorage(),
      generator: UnsupportedDeviceIdentityGenerator(),
      signer: UnsupportedDeviceAuthSigner(),
    );
  }

  final SecureStorage _storage;
  final DeviceIdentityGenerator _generator;
  final DeviceAuthSigner _signer;

  Future<DevicePublicIdentity?> loadDevice() async {
    final deviceId = await _storage.read(SecureStorageKeys.deviceId);
    final devicePublicKey = await _storage.read(SecureStorageKeys.devicePublicKey);
    final signingKeySecret =
        await _storage.read(SecureStorageKeys.deviceSigningKeySecret);

    if (deviceId == null && devicePublicKey == null && signingKeySecret == null) {
      return null;
    }
    if (deviceId == null || devicePublicKey == null || signingKeySecret == null) {
      throw const DeviceIdentityCorrupted();
    }

    return DevicePublicIdentity(
      deviceId: deviceId,
      devicePublicKey: devicePublicKey,
    );
  }

  Future<DevicePublicIdentity> loadOrCreateDevice() async {
    final existing = await loadDevice();
    if (existing != null) {
      return existing;
    }

    final generated = await _generator.generate();
    await _storage.write(
      SecureStorageKeys.deviceId,
      generated.publicIdentity.deviceId,
    );
    await _storage.write(
      SecureStorageKeys.devicePublicKey,
      generated.publicIdentity.devicePublicKey,
    );
    await _storage.write(
      SecureStorageKeys.deviceSigningKeySecret,
      generated.signingKeySecret,
    );

    return generated.publicIdentity;
  }

  Future<void> deleteDevice() async {
    await _storage.delete(SecureStorageKeys.deviceId);
    await _storage.delete(SecureStorageKeys.devicePublicKey);
    await _storage.delete(SecureStorageKeys.deviceSigningKeySecret);
  }

  Future<DevicePublicIdentity> rotateDevice() async {
    await deleteDevice();
    return loadOrCreateDevice();
  }

  Future<DeviceSignature> signAuthInput(Uint8List canonicalInput) async {
    final signingKeySecret =
        await _storage.read(SecureStorageKeys.deviceSigningKeySecret);
    if (signingKeySecret == null) {
      throw const DeviceIdentityUnavailable();
    }

    // 安全边界：签名 secret 只在 secure storage、manager 和 signer adapter 内流动；
    // 调用方只能得到 wire signature，不能拿到原始 signing secret。
    return _signer.sign(
      signingKeySecret: signingKeySecret,
      canonicalInput: Uint8List.unmodifiable(canonicalInput),
    );
  }
}
