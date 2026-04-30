abstract class NativeError implements Exception {
  const NativeError(this.code, this.safeMessage);

  final String code;
  final String safeMessage;

  @override
  String toString() => '$code: $safeMessage';
}

const _sensitiveErrorFragments = <String>[
  'pairing_token',
  'server_private_key',
  'terminal_transcript',
  'terminal transcript',
  'session_data',
  'pty_output',
  'token=',
  'private=',
  'signature=',
  'ciphertext=',
  'terminal-secret',
];

String _sanitizeNativeErrorMessage(String message) {
  final normalized = message.toLowerCase();
  for (final fragment in _sensitiveErrorFragments) {
    if (normalized.contains(fragment)) {
      // 动态校验错误可能拼接用户输入；命中敏感片段时只返回稳定安全文案。
      return '输入不合法，敏感内容已隐藏。';
    }
  }
  return message;
}

class SecureStorageUnavailable extends NativeError {
  const SecureStorageUnavailable()
      : super(
          'secure_storage_unavailable',
          '当前平台未接入安全存储，production 路径不会降级到明文持久化。',
        );
}

class DeviceIdentityUnavailable extends NativeError {
  const DeviceIdentityUnavailable()
      : super(
          'device_identity_unavailable',
          '当前没有可用的本机设备身份。',
        );
}

class DeviceIdentityCorrupted extends NativeError {
  const DeviceIdentityCorrupted()
      : super(
          'device_identity_corrupted',
          '本机设备身份状态不完整，请删除后重新配对。',
        );
}

class DeviceSigningUnsupported extends NativeError {
  const DeviceSigningUnsupported()
      : super(
          'device_signing_unsupported',
          'Native 骨架尚未接入真实 Ed25519 签名实现。',
        );
}

class NativeStateCorrupted extends NativeError {
  const NativeStateCorrupted()
      : super(
          'native_state_corrupted',
          '本地公开状态无法解析。',
        );
}

class ProtocolClientUnavailable extends NativeError {
  const ProtocolClientUnavailable()
      : super(
          'protocol_client_unavailable',
          'Native 骨架尚未接入 direct WebSocket/E2EE 客户端。',
        );
}

class NativeValidationError extends NativeError {
  NativeValidationError(String message)
      : super('native_validation_error', _sanitizeNativeErrorMessage(message));
}
