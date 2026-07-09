# termui Native

`termui/native` 是 termd 的 Flutter Native 客户端骨架。当前 item 只固定 Native UI、service、secure storage 和 device key 管理边界，不实现真实 WebSocket、E2EE、auth 或 terminal renderer。

## 当前边界

- UI 只能调用 `TermuiNativeService`，不得直接读写设备签名材料。
- production secure storage 当前显式返回 unsupported，不会降级到明文文件、平台偏好存储或浏览器存储。
- test-only fake storage 只放在 `test/**`，用于验证 key manager 与 paired server 状态边界。
- 一次性配对码只作为内存输入交给 service，当前骨架不会持久化，也不会冒充已建立真实配对。
- 已配对 daemon 状态只保存公开身份、剥离 query/fragment 后的安全 URL、配对时间和本机公开设备身份。
- `device_public_key`、`device_signing_key_secret`、`daemon_public_key` 当前只接受 `ed25519-v1:` 前缀；损坏状态必须 fail closed。

## 后续接入

有 Flutter SDK 的环境可运行：

```bash
flutter pub get
flutter analyze
flutter test
```

后续实现真实 Native client 前，应先用 Dart/Rust 兼容测试锁定 auth canonical input、wire prefix、E2EE sequence/AAD 和 base64 行为。
