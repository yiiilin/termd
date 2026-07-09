import 'package:flutter_test/flutter_test.dart';
import 'package:termui_native/core/device/paired_server.dart';
import 'package:termui_native/core/errors/native_error.dart';
import 'package:termui_native/core/storage/secure_storage_keys.dart';

import '../support/fake_secure_storage.dart';

void main() {
  test('paired server store persists only public daemon and device state', () async {
    final storage = FakeSecureStorage();
    final store = PairedServerStore(storage);

    await store.recordPairedServer(_server());

    final raw = storage.values[SecureStorageKeys.pairedServers]!;
    final servers = await store.listPairedServers();

    expect(servers.single.serverId, 'server-test-1');
    expect(raw, contains('daemon_public_key'));
    expect(raw, contains('device_public_key'));

    // 配对 token、daemon 私钥和终端明文都不属于 Native 可持久化状态。
    const forbiddenFragments = <String>[
      'relay_token',
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

  test('paired server store rejects malformed json state', () async {
    final storage = FakeSecureStorage(<String, String>{
      SecureStorageKeys.pairedServers: '{not-json',
    });
    final store = PairedServerStore(storage);

    await expectLater(
      store.listPairedServers(),
      throwsA(isA<NativeStateCorrupted>()),
    );
  });

  test('paired server store rejects present but empty state', () async {
    final storage = FakeSecureStorage(<String, String>{
      SecureStorageKeys.pairedServers: '',
    });
    final store = PairedServerStore(storage);

    await expectLater(
      store.listPairedServers(),
      throwsA(isA<NativeStateCorrupted>()),
    );
  });

  test('paired server store rejects present empty server list', () async {
    final storage = FakeSecureStorage(<String, String>{
      SecureStorageKeys.pairedServers: '[]',
    });
    final store = PairedServerStore(storage);

    await expectLater(
      store.listPairedServers(),
      throwsA(isA<NativeStateCorrupted>()),
    );
  });

  test('paired server store rejects entries with missing fields', () async {
    final storage = FakeSecureStorage(<String, String>{
      SecureStorageKeys.pairedServers: '[{"server_id":"server-test-1"}]',
    });
    final store = PairedServerStore(storage);

    await expectLater(
      store.listPairedServers(),
      throwsA(isA<NativeStateCorrupted>()),
    );
  });

  test('paired server store rejects non-map list entries', () async {
    final storage = FakeSecureStorage(<String, String>{
      SecureStorageKeys.pairedServers: '["server-test-1"]',
    });
    final store = PairedServerStore(storage);

    await expectLater(
      store.listPairedServers(),
      throwsA(isA<NativeStateCorrupted>()),
    );
  });

  test('paired server store rejects duplicate server IDs in persisted state', () async {
    final storage = FakeSecureStorage(<String, String>{
      SecureStorageKeys.pairedServers:
          '[${_serverJson('server-test-1')},${_serverJson('server-test-1')}]',
    });
    final store = PairedServerStore(storage);

    await expectLater(
      store.listPairedServers(),
      throwsA(isA<NativeStateCorrupted>()),
    );
  });

  test('paired server store rejects multiple persisted daemon IDs', () async {
    final storage = FakeSecureStorage(<String, String>{
      SecureStorageKeys.pairedServers:
          '[${_serverJson('server-test-1')},${_serverJson('server-test-2')}]',
    });
    final store = PairedServerStore(storage);

    await expectLater(
      store.listPairedServers(),
      throwsA(isA<NativeStateCorrupted>()),
    );
  });

  test('paired server store strips query and fragment from persisted URLs', () async {
    final storage = FakeSecureStorage();
    final store = PairedServerStore(storage);

    await store.recordPairedServer(
      _server(
        url:
            '  WSS://Example.COM:443/shell/../ws?relay_token=relay-secret#pairing-fragment  ',
      ),
    );

    final servers = await store.listPairedServers();

    expect(servers.single.url, 'wss://example.com:443/ws');
    expect(
      storage.values[SecureStorageKeys.pairedServers],
      contains('"url":"wss://example.com:443/ws"'),
    );
    expect(
      storage.values[SecureStorageKeys.pairedServers],
      isNot(contains('relay_token')),
    );
    expect(
      storage.values[SecureStorageKeys.pairedServers],
      isNot(contains('pairing-fragment')),
    );
  });

  test('paired server store rejects URLs with persisted credentials', () async {
    final storage = FakeSecureStorage();
    final store = PairedServerStore(storage);

    await expectLater(
      store.recordPairedServer(_server(url: 'wss://user:pass@example.test/ws')),
      throwsA(isA<NativeValidationError>()),
    );
  });

  test('paired server store validates fields before writing state', () async {
    final storage = FakeSecureStorage();
    final store = PairedServerStore(storage);

    await expectLater(
      store.recordPairedServer(_server(serverId: '')),
      throwsA(isA<NativeValidationError>()),
    );
    expect(storage.values[SecureStorageKeys.pairedServers], isNull);
  });

  test('paired server store rejects malformed public key material before writing', () async {
    final storage = FakeSecureStorage();
    final store = PairedServerStore(storage);

    await expectLater(
      store.recordPairedServer(
        _server(daemonPublicKey: 'daemon-public-key-without-prefix'),
      ),
      throwsA(isA<NativeValidationError>()),
    );
    await expectLater(
      store.recordPairedServer(_server(devicePublicKey: 'device-public-key-without-prefix')),
      throwsA(isA<NativeValidationError>()),
    );
    expect(storage.values[SecureStorageKeys.pairedServers], isNull);
  });

  test('paired server store rejects corrupted persisted public key material', () async {
    for (final rawState in <String>[
      '''
[
  {
    "server_id": "server-test-1",
    "daemon_public_key": "daemon-public-key-without-prefix",
    "url": "ws://127.0.0.1:8765/ws",
    "paired_at_ms": 1710000000000,
    "device_id": "device-test-1",
    "device_public_key": "ed25519-v1:device-public-1"
  }
]
''',
      '''
[
  {
    "server_id": "server-test-1",
    "daemon_public_key": "ed25519-v1:daemon-public-key",
    "url": "ws://127.0.0.1:8765/ws",
    "paired_at_ms": 1710000000000,
    "device_id": "device-test-1",
    "device_public_key": "device-public-key-without-prefix"
  }
]
''',
    ]) {
      final storage = FakeSecureStorage(<String, String>{
        SecureStorageKeys.pairedServers: rawState,
      });
      final store = PairedServerStore(storage);

      await expectLater(
        store.listPairedServers(),
        throwsA(isA<NativeStateCorrupted>()),
      );
    }
  });

  test('paired server store enforces native single-daemon mode', () async {
    final storage = FakeSecureStorage();
    final store = PairedServerStore(storage);

    await store.recordPairedServer(_server(serverId: 'server-test-1'));

    await expectLater(
      store.recordPairedServer(_server(serverId: 'server-test-2')),
      throwsA(isA<NativeValidationError>()),
    );
  });
}

PairedServer _server({
  String serverId = 'server-test-1',
  String url = 'ws://127.0.0.1:8765/ws',
  String daemonPublicKey = 'ed25519-v1:daemon-public-key',
  String devicePublicKey = 'ed25519-v1:device-public-1',
}) {
  return PairedServer(
    serverId: serverId,
    daemonPublicKey: daemonPublicKey,
    url: url,
    pairedAtMs: 1710000000000,
    deviceId: 'device-test-1',
    devicePublicKey: devicePublicKey,
  );
}

String _serverJson(String serverId) {
  return '''
{
  "server_id": "$serverId",
  "daemon_public_key": "ed25519-v1:daemon-public-key",
  "url": "ws://127.0.0.1:8765/ws",
  "paired_at_ms": 1710000000000,
  "device_id": "device-test-1",
  "device_public_key": "ed25519-v1:device-public-1"
}
''';
}
