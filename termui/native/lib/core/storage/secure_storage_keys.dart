final class SecureStorageKeys {
  const SecureStorageKeys._();

  static const deviceId = 'termd.device_id';
  static const devicePublicKey = 'termd.device_public_key';
  static const deviceSigningKeySecret = 'termd.device_signing_key_secret';
  static const pairedServers = 'termd.paired_servers';

  /// 允许持久化的 key 集中定义，便于审核确认没有保存配对码、daemon 私钥或终端明文。
  static const all = <String>[
    deviceId,
    devicePublicKey,
    deviceSigningKeySecret,
    pairedServers,
  ];

  static bool isManagedKey(String key) => all.contains(key);
}
