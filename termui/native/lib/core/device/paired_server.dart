import 'dart:convert';

import '../errors/native_error.dart';
import '../storage/secure_storage.dart';
import '../storage/secure_storage_keys.dart';

final class PairedServer {
  const PairedServer({
    required this.serverId,
    required this.daemonPublicKey,
    required this.url,
    required this.pairedAtMs,
    required this.deviceId,
    required this.devicePublicKey,
  });

  final String serverId;
  final String daemonPublicKey;
  final String url;
  final int pairedAtMs;
  final String deviceId;
  final String devicePublicKey;

  Map<String, Object?> toJson() => {
        'server_id': serverId,
        'daemon_public_key': daemonPublicKey,
        'url': url,
        'paired_at_ms': pairedAtMs,
        'device_id': deviceId,
        'device_public_key': devicePublicKey,
      };

  static PairedServer fromJson(Map<String, Object?> json) {
    return PairedServer(
      serverId: json['server_id'] as String,
      daemonPublicKey: json['daemon_public_key'] as String,
      url: json['url'] as String,
      pairedAtMs: json['paired_at_ms'] as int,
      deviceId: json['device_id'] as String,
      devicePublicKey: json['device_public_key'] as String,
    );
  }
}

final class PairedServerStore {
  const PairedServerStore(this._storage);

  final SecureStorage _storage;

  Future<List<PairedServer>> listPairedServers() async {
    final raw = await _storage.read(SecureStorageKeys.pairedServers);
    if (raw == null || raw.isEmpty) {
      return const [];
    }

    final decoded = jsonDecode(raw);
    if (decoded is! List<Object?>) {
      throw const NativeStateCorrupted();
    }

    return [
      for (final item in decoded)
        if (item is Map<String, Object?>) PairedServer.fromJson(item),
    ];
  }

  Future<void> recordPairedServer(PairedServer server) async {
    final servers = await listPairedServers();
    final next = <PairedServer>[
      for (final existing in servers)
        if (existing.serverId != server.serverId) existing,
      server,
    ];

    // 持久化字段只包含 daemon 公开身份、URL、配对时间和本机公开设备身份。
    await _storage.write(
      SecureStorageKeys.pairedServers,
      jsonEncode([for (final item in next) item.toJson()]),
    );
  }

  Future<void> deleteAll() async {
    await _storage.delete(SecureStorageKeys.pairedServers);
  }
}
