import 'package:flutter_test/flutter_test.dart';
import 'package:termui_native/core/errors/native_error.dart';
import 'package:termui_native/core/storage/secure_storage.dart';

void main() {
  test('unsupported secure storage fails closed for every operation', () async {
    const storage = UnsupportedSecureStorage();

    // production 尚未接入平台安全存储时，读写删都必须失败，不能降级到明文 fallback。
    await expectLater(
      storage.read('termd.device_id'),
      throwsA(isA<SecureStorageUnavailable>()),
    );
    await expectLater(
      storage.write('termd.device_id', 'device-test-1'),
      throwsA(isA<SecureStorageUnavailable>()),
    );
    await expectLater(
      storage.delete('termd.device_id'),
      throwsA(isA<SecureStorageUnavailable>()),
    );
  });
}
