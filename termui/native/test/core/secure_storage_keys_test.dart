import 'package:flutter_test/flutter_test.dart';
import 'package:termui_native/core/storage/secure_storage_keys.dart';

void main() {
  test('managed secure storage keys are intentionally narrow', () {
    expect(
      SecureStorageKeys.all,
      containsAll(<String>[
        'termd.device_id',
        'termd.device_public_key',
        'termd.device_signing_key_secret',
        'termd.paired_servers',
      ]),
    );
  });

  test('managed secure storage keys exclude unsafe persistence categories', () {
    final joined = SecureStorageKeys.all.join('\n');

    const unsafeFragments = <String>[
      'pairing_token',
      'server_private_key',
      'terminal_transcript',
      'terminal transcript',
      'session_data',
      'pty_output',
      'token',
      'private',
      'history',
      'transcript',
      'pty',
    ];

    for (final fragment in unsafeFragments) {
      expect(joined, isNot(contains(fragment)));
    }
  });

  test('managed secure storage keys stay on the reviewed allowlist', () {
    // 新增持久化 key 必须经过安全审核，避免把 token、终端输出或 daemon 私钥写入本机。
    expect(SecureStorageKeys.all, <String>[
      SecureStorageKeys.deviceId,
      SecureStorageKeys.devicePublicKey,
      SecureStorageKeys.deviceSigningKeySecret,
      SecureStorageKeys.pairedServers,
    ]);
  });
}
