import 'dart:convert';
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
    final record = await _loadIdentityRecord();
    return record?.publicIdentity;
  }

  Future<DevicePublicIdentity> loadOrCreateDevice() async {
    final existing = await loadDevice();
    if (existing != null) {
      return existing;
    }

    final generated = await _generator.generate();
    await _writeIdentityRecord(
      _DeviceIdentityRecord(
        publicIdentity: generated.publicIdentity,
        signingKeySecret: generated.signingKeySecret,
      ),
    );
    await _deleteLegacyComponentKeys();

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
    final record = await _loadIdentityRecord();
    if (record == null) {
      throw const DeviceIdentityUnavailable();
    }

    // 安全边界：签名 secret 只在 secure storage、manager 和 signer adapter 内流动；
    // 调用方只能得到 wire signature，不能拿到原始 signing secret。
    return _signer.sign(
      signingKeySecret: record.signingKeySecret,
      canonicalInput: Uint8List.unmodifiable(canonicalInput),
    );
  }

  Future<_DeviceIdentityRecord?> _loadIdentityRecord() async {
    final rawRecord = await _storage.read(SecureStorageKeys.deviceId);
    if (rawRecord != null && _looksLikeJsonObject(rawRecord)) {
      final record = _DeviceIdentityRecord.fromJson(
        _decodeIdentityRecordJson(rawRecord),
      );
      await _deleteLegacyComponentKeys();
      return record;
    }

    final legacyPublicKey =
        await _storage.read(SecureStorageKeys.devicePublicKey);
    final legacySigningKeySecret =
        await _storage.read(SecureStorageKeys.deviceSigningKeySecret);

    if (rawRecord == null &&
        legacyPublicKey == null &&
        legacySigningKeySecret == null) {
      return null;
    }

    if (rawRecord != null &&
        legacyPublicKey != null &&
        legacySigningKeySecret != null) {
      final record = _DeviceIdentityRecord(
        publicIdentity: DevicePublicIdentity(
          deviceId: rawRecord,
          devicePublicKey: legacyPublicKey,
        ),
        signingKeySecret: legacySigningKeySecret,
      );
      // 旧版本分三次写入身份材料；读取到完整旧状态时立即收敛为单条版本化记录。
      await _writeIdentityRecord(record);
      await _deleteLegacyComponentKeys();
      return record;
    }

    throw const DeviceIdentityCorrupted();
  }

  Future<void> _writeIdentityRecord(_DeviceIdentityRecord record) {
    return _storage.write(
      SecureStorageKeys.deviceId,
      jsonEncode(record.toJson()),
    );
  }

  Future<void> _deleteLegacyComponentKeys() async {
    await _storage.delete(SecureStorageKeys.devicePublicKey);
    await _storage.delete(SecureStorageKeys.deviceSigningKeySecret);
  }
}

const _deviceIdentityRecordVersion = 1;

final class _DeviceIdentityRecord {
  const _DeviceIdentityRecord({
    required this.publicIdentity,
    required this.signingKeySecret,
  });

  final DevicePublicIdentity publicIdentity;
  final String signingKeySecret;

  Map<String, Object?> toJson() => {
        'version': _deviceIdentityRecordVersion,
        'device_id': publicIdentity.deviceId,
        'device_public_key': publicIdentity.devicePublicKey,
        'device_signing_key_secret': signingKeySecret,
      };

  static _DeviceIdentityRecord fromJson(Map<String, Object?> json) {
    final version = json['version'];
    if (version != _deviceIdentityRecordVersion) {
      throw const DeviceIdentityCorrupted();
    }

    return _DeviceIdentityRecord(
      publicIdentity: DevicePublicIdentity(
        deviceId: _readIdentityString(json, 'device_id'),
        devicePublicKey: _readIdentityString(json, 'device_public_key'),
      ),
      signingKeySecret: _readIdentityString(
        json,
        'device_signing_key_secret',
      ),
    );
  }
}

bool _looksLikeJsonObject(String value) => value.trimLeft().startsWith('{');

Map<String, Object?> _decodeIdentityRecordJson(String rawRecord) {
  try {
    final decoded = jsonDecode(rawRecord);
    if (decoded is! Map<Object?, Object?>) {
      throw const DeviceIdentityCorrupted();
    }

    final json = <String, Object?>{};
    for (final entry in decoded.entries) {
      final key = entry.key;
      if (key is! String) {
        throw const DeviceIdentityCorrupted();
      }
      json[key] = entry.value;
    }
    return json;
  } on NativeError {
    rethrow;
  } on Object {
    throw const DeviceIdentityCorrupted();
  }
}

String _readIdentityString(Map<String, Object?> json, String key) {
  final value = json[key];
  if (value is! String || value.isEmpty) {
    throw const DeviceIdentityCorrupted();
  }
  return value;
}
