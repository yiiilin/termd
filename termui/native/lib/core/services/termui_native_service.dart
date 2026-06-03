import '../device/device_identity.dart';
import '../device/device_key_manager.dart';
import '../device/paired_server.dart';
import '../errors/native_error.dart';
import '../protocol/protocol_client.dart';
import '../protocol/protocol_types.dart';
import '../storage/secure_storage.dart';

final class WorkbenchSnapshot {
  const WorkbenchSnapshot({
    required this.device,
    required this.pairedServers,
    required this.statusMessage,
  });

  final DevicePublicIdentity? device;
  final List<PairedServer> pairedServers;
  final String statusMessage;
}

final class PairingPreparation {
  const PairingPreparation({
    required this.device,
    required this.url,
  });

  final DevicePublicIdentity device;
  final String url;
}

final class TermuiNativeService {
  const TermuiNativeService({
    required DeviceKeyManager deviceKeyManager,
    required PairedServerStore pairedServerStore,
    required TermdProtocolClient protocolClient,
  })  : _deviceKeyManager = deviceKeyManager,
        _pairedServerStore = pairedServerStore,
        _protocolClient = protocolClient;

  factory TermuiNativeService.productionUnsupported() {
    final keyManager = DeviceKeyManager.productionUnsupported();
    return TermuiNativeService(
      deviceKeyManager: keyManager,
      pairedServerStore: PairedServerStore(
        // 与 key manager 使用同样的 production unsupported 边界；后续真实 adapter 接入时替换。
        const UnsupportedSecureStorage(),
      ),
      protocolClient: const UnsupportedTermdProtocolClient(),
    );
  }

  final DeviceKeyManager _deviceKeyManager;
  final PairedServerStore _pairedServerStore;
  final TermdProtocolClient _protocolClient;

  Future<WorkbenchSnapshot> loadWorkbenchSnapshot() async {
    try {
      final device = await _deviceKeyManager.loadDevice();
      final servers = await _pairedServerStore.listPairedServers();
      return WorkbenchSnapshot(
        device: device,
        pairedServers: servers,
        statusMessage: 'ready',
      );
    } on NativeError catch (error) {
      return WorkbenchSnapshot(
        device: null,
        pairedServers: const [],
        statusMessage: error.safeMessage,
      );
    }
  }

  Future<PairingPreparation> preparePairing({
    required String url,
    required String oneTimePairingCode,
  }) async {
    final normalizedUrl = normalizePairedServerUrl(url);
    final code = oneTimePairingCode.trim();
    if (code.isEmpty) {
      throw NativeValidationError('需要一次性配对码。');
    }

    // 配对码只在本方法栈上短暂停留，当前骨架不记录、不缓存、不写 secure storage。
    final device = await _deviceKeyManager.loadOrCreateDevice();
    return PairingPreparation(device: device, url: normalizedUrl);
  }

  Future<void> recordPairedServer(PairedServer server) {
    return _pairedServerStore.recordPairedServer(server);
  }

  Future<List<RemoteSessionSummary>> listSessions() {
    return _protocolClient.listSessions();
  }

  Future<ProtocolConnectionStatus> connectionStatus() {
    return _protocolClient.connectionStatus();
  }

  Future<TerminalAttachPreview> prepareTerminalAttach(String sessionId) {
    return _protocolClient.prepareAttach(sessionId);
  }
}
