import 'package:flutter_test/flutter_test.dart';
import 'package:termui_native/core/errors/native_error.dart';

void main() {
  test('fixed native errors expose only redacted safe messages', () {
    final errors = <NativeError>[
      const SecureStorageUnavailable(),
      const DeviceIdentityUnavailable(),
      const DeviceIdentityCorrupted(),
      const DeviceSigningUnsupported(),
      const NativeStateCorrupted(),
      const ProtocolClientUnavailable(),
    ];

    for (final error in errors) {
      _expectSafeNativeError(error);
    }
  });

  test('dynamic validation errors redact sensitive fragments', () {
    final error = NativeValidationError(
      'pairing_token=PAIRING-TOKEN-1 signature=SIG ciphertext=CIPHERTEXT',
    );

    _expectSafeNativeError(error);
    expect(error.safeMessage, '输入不合法，敏感内容已隐藏。');
  });

  test('dynamic validation errors redact generic secret query fragments', () {
    final error = NativeValidationError(
      'url=wss://example.test/ws?secret=PAIR-CODE-1',
    );

    _expectSafeNativeError(error);
    expect(error.safeMessage, '输入不合法，敏感内容已隐藏。');
  });
}

void _expectSafeNativeError(NativeError error) {
  final rendered = '${error.code}\n${error.safeMessage}\n$error'.toLowerCase();

  // 这些 sentinel 代表 token、私钥、签名、密文和终端明文；错误对象不得回显原始内容。
  const forbiddenFragments = <String>[
    'pairing-token-1',
    'pairing_token=',
    'server_private_key',
    'terminal_transcript',
    'terminal transcript',
    'terminal-secret-output',
    'session_data',
    'pty_output',
    'signature=',
    'ciphertext=',
    'secret=',
  ];

  for (final fragment in forbiddenFragments) {
    expect(rendered, isNot(contains(fragment)));
  }
}
