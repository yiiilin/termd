import 'dart:convert';

import '../errors/native_error.dart';
import '../storage/secure_storage.dart';
import '../storage/secure_storage_keys.dart';

const _ed25519WirePrefix = 'ed25519-v1:';

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
    try {
      return PairedServer(
        serverId: _readServerString(json, 'server_id'),
        daemonPublicKey: _readServerPublicKey(json, 'daemon_public_key'),
        url: normalizePairedServerUrl(_readServerString(json, 'url')),
        pairedAtMs: _readServerInt(json, 'paired_at_ms'),
        deviceId: _readServerString(json, 'device_id'),
        devicePublicKey: _readServerPublicKey(json, 'device_public_key'),
      );
    } on NativeValidationError {
      throw const NativeStateCorrupted();
    } on NativeError {
      rethrow;
    } on Object {
      throw const NativeStateCorrupted();
    }
  }
}

final class PairedServerStore {
  const PairedServerStore(this._storage);

  final SecureStorage _storage;

  Future<List<PairedServer>> listPairedServers() async {
    final raw = await _storage.read(SecureStorageKeys.pairedServers);
    if (raw == null) {
      return const [];
    }
    if (raw.isEmpty) {
      throw const NativeStateCorrupted();
    }

    final decoded = _decodePairedServerList(raw);
    if (decoded.isEmpty) {
      throw const NativeStateCorrupted();
    }

    final seenServerIds = <String>{};
    final servers = <PairedServer>[];
    for (final item in decoded) {
      if (item is! Map<Object?, Object?>) {
        throw const NativeStateCorrupted();
      }

      final server = PairedServer.fromJson(_jsonObjectFromDecodedMap(item));
      if (!seenServerIds.add(server.serverId)) {
        throw const NativeStateCorrupted();
      }
      if (seenServerIds.length > 1) {
        // Native UI 暂无 daemon 选择器；持久化多 daemon 状态会让协议调用缺少明确 server scope。
        throw const NativeStateCorrupted();
      }
      servers.add(server);
    }

    return servers;
  }

  Future<void> recordPairedServer(PairedServer server) async {
    final servers = await listPairedServers();
    if (servers.any(
      (PairedServer existing) => existing.serverId != server.serverId,
    )) {
      // Native UI 当前没有 daemon 选择器；store 层先强制单 daemon，避免协议调用缺少 server scope。
      throw NativeValidationError('Native 当前只支持一个 daemon，请先删除现有配对。');
    }

    final normalizedServer = _validateServerForWrite(server);
    final next = <PairedServer>[
      for (final existing in servers)
        if (existing.serverId != normalizedServer.serverId) existing,
      normalizedServer,
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

String normalizePairedServerUrl(String rawUrl) {
  final parsed = Uri.tryParse(rawUrl.trim());
  final scheme = parsed?.scheme.toLowerCase();
  if (parsed == null ||
      (scheme != 'ws' && scheme != 'wss') ||
      parsed.host.isEmpty ||
      parsed.userInfo.isNotEmpty) {
    throw NativeValidationError('daemon URL 必须是 ws 或 wss，并且包含 host。');
  }

  // Native 只保存连接定位信息；为避免 relay_token 等 secret query/fragment 落盘或回显，
  // 这里统一剥离 query 与 fragment。
  final normalized = parsed.normalizePath();
  return Uri(
    scheme: normalized.scheme.toLowerCase(),
    host: normalized.host.toLowerCase(),
    port: normalized.hasPort ? normalized.port : null,
    path: normalized.path,
  ).toString();
}

List<Object?> _decodePairedServerList(String raw) {
  try {
    final decoded = jsonDecode(raw);
    if (decoded is! List<Object?>) {
      throw const NativeStateCorrupted();
    }
    return decoded;
  } on NativeError {
    rethrow;
  } on Object {
    throw const NativeStateCorrupted();
  }
}

Map<String, Object?> _jsonObjectFromDecodedMap(Map<Object?, Object?> decoded) {
  final json = <String, Object?>{};
  for (final entry in decoded.entries) {
    final key = entry.key;
    if (key is! String) {
      throw const NativeStateCorrupted();
    }
    json[key] = entry.value;
  }
  return json;
}

String _readServerString(Map<String, Object?> json, String key) {
  final value = json[key];
  if (value is! String || value.isEmpty) {
    throw const NativeStateCorrupted();
  }
  return value;
}

String _readServerPublicKey(Map<String, Object?> json, String key) {
  final value = _readServerString(json, key);
  if (!value.startsWith(_ed25519WirePrefix)) {
    throw const NativeStateCorrupted();
  }
  return value;
}

int _readServerInt(Map<String, Object?> json, String key) {
  final value = json[key];
  if (value is! int) {
    throw const NativeStateCorrupted();
  }
  return value;
}

PairedServer _validateServerForWrite(PairedServer server) {
  _requireNonEmptyServerField(server.serverId);
  _requireNonEmptyServerField(server.deviceId);
  _requireEd25519WireField(server.daemonPublicKey, 'daemon_public_key');
  _requireEd25519WireField(server.devicePublicKey, 'device_public_key');
  return PairedServer(
    serverId: server.serverId,
    daemonPublicKey: server.daemonPublicKey,
    url: normalizePairedServerUrl(server.url),
    pairedAtMs: server.pairedAtMs,
    deviceId: server.deviceId,
    devicePublicKey: server.devicePublicKey,
  );
}

void _requireNonEmptyServerField(String value) {
  if (value.isEmpty) {
    throw NativeValidationError('配对 daemon 状态字段不能为空。');
  }
}

void _requireEd25519WireField(String value, String fieldName) {
  if (!value.startsWith(_ed25519WirePrefix)) {
    throw NativeValidationError('$fieldName 必须使用 ed25519-v1: 前缀。');
  }
}
