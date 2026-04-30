final class DevicePublicIdentity {
  const DevicePublicIdentity({
    required this.deviceId,
    required this.devicePublicKey,
  });

  final String deviceId;
  final String devicePublicKey;

  Map<String, Object?> toJson() => {
        'device_id': deviceId,
        'device_public_key': devicePublicKey,
      };

  static DevicePublicIdentity fromJson(Map<String, Object?> json) {
    return DevicePublicIdentity(
      deviceId: json['device_id'] as String,
      devicePublicKey: json['device_public_key'] as String,
    );
  }
}

final class DeviceSignature {
  const DeviceSignature(this.wireValue);

  final String wireValue;
}
