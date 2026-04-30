import 'package:flutter_test/flutter_test.dart';
import 'package:termui_native/core/device/paired_server.dart';
import 'package:termui_native/core/storage/secure_storage.dart';
import 'package:termui_native/core/storage/secure_storage_keys.dart';

void main() {
  test('paired server store persists only public daemon and device state', () async {
    final storage = FakeSecureStorage();
    final store = PairedServerStore(storage);

    await store.recordPairedServer(
      const PairedServer(
        serverId: 'server-test-1',
        daemonPublicKey: 'daemon-public-key',
        url: 'ws://127.0.0.1:8765/ws',
        pairedAtMs: 1710000000000,
        deviceId: 'device-test-1',
        devicePublicKey: 'ed25519-v1:device-public-1',
      ),
    );

    final raw = storage.values[SecureStorageKeys.pairedServers]!;
    final servers = await store.listPairedServers();

    expect(servers.single.serverId, 'server-test-1');
    expect(raw, contains('daemon_public_key'));
    expect(raw, contains('device_public_key'));

    // 配对 token、daemon 私钥和终端明文都不属于 Native 可持久化状态。
    const forbiddenFragments = <String>[
      'pairing_token',
      'server_private_key',
      'terminal_transcript',
      'session_data',
      'pty_output',
      'PAIR-CODE-SHOULD-STAY-IN-MEMORY',
      'TERMINAL-OUTPUT-SHOULD-STAY-EPHEMERAL',
    ];
    for (final fragment in forbiddenFragments) {
      expect(raw, isNot(contains(fragment)));
    }
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
