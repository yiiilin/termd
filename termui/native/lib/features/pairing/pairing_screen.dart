import 'package:flutter/material.dart';

import '../../core/errors/native_error.dart';
import '../../core/services/termui_native_service.dart';

class PairingScreen extends StatefulWidget {
  const PairingScreen({
    required this.service,
    super.key,
  });

  final TermuiNativeService service;

  @override
  State<PairingScreen> createState() => _PairingScreenState();
}

class _PairingScreenState extends State<PairingScreen> {
  final TextEditingController _urlController = TextEditingController(
    text: 'ws://127.0.0.1:8765/ws',
  );
  final TextEditingController _codeController = TextEditingController();
  String _status = '未配对';
  bool _busy = false;

  @override
  void dispose() {
    _urlController.dispose();
    _codeController.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    return ListView(
      padding: const EdgeInsets.all(24),
      children: [
        Text('配对', style: Theme.of(context).textTheme.headlineMedium),
        const SizedBox(height: 18),
        TextField(
          controller: _urlController,
          decoration: const InputDecoration(
            labelText: 'Daemon URL',
            border: OutlineInputBorder(),
          ),
        ),
        const SizedBox(height: 12),
        TextField(
          controller: _codeController,
          decoration: const InputDecoration(
            labelText: '一次性配对码',
            border: OutlineInputBorder(),
          ),
          obscureText: true,
        ),
        const SizedBox(height: 16),
        FilledButton.icon(
          onPressed: _busy ? null : _preparePairing,
          icon: const Icon(Icons.key_rounded),
          label: Text(_busy ? '处理中' : '准备配对'),
        ),
        const SizedBox(height: 18),
        Text(_status),
      ],
    );
  }

  Future<void> _preparePairing() async {
    setState(() => _busy = true);
    try {
      final preview = await widget.service.preparePairing(
        url: _urlController.text,
        oneTimePairingCode: _codeController.text,
      );
      if (!mounted) {
        return;
      }
      setState(() {
        _status = preview.statusMessage;
      });
    } on NativeError catch (error) {
      if (!mounted) {
        return;
      }
      setState(() => _status = error.safeMessage);
    } finally {
      // 安全边界：一次性配对码只在内存中使用，提交后立即清空输入框。
      if (mounted) {
        _codeController.clear();
        setState(() => _busy = false);
      }
    }
  }
}
