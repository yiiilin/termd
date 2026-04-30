import '../errors/native_error.dart';

abstract interface class SecureStorage {
  Future<String?> read(String key);

  Future<void> write(String key, String value);

  Future<void> delete(String key);
}

final class UnsupportedSecureStorage implements SecureStorage {
  const UnsupportedSecureStorage();

  @override
  Future<String?> read(String key) => _unavailable();

  @override
  Future<void> write(String key, String value) => _unavailable();

  @override
  Future<void> delete(String key) => _unavailable();

  Future<T> _unavailable<T>() async {
    // 安全边界：production 没有平台 secure storage 时必须失败，
    // 不能静默改用明文文件、平台偏好存储、SQLite 或浏览器存储。
    throw const SecureStorageUnavailable();
  }
}
