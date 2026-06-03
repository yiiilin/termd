import 'package:termui_native/core/storage/secure_storage.dart';

final class FakeSecureStorage implements SecureStorage {
  FakeSecureStorage([Map<String, String>? initialValues])
      : values = <String, String>{...?initialValues};

  final Map<String, String> values;

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
