import 'package:flutter/material.dart';

import '../../core/errors/native_error.dart';
import '../../core/services/termui_native_service.dart';

class TerminalScreen extends StatefulWidget {
  const TerminalScreen({
    required this.service,
    super.key,
  });

  final TermuiNativeService service;

  @override
  State<TerminalScreen> createState() => _TerminalScreenState();
}

class _TerminalScreenState extends State<TerminalScreen> {
  final TextEditingController _sessionController = TextEditingController();
  String _status = '未 attach';
  bool _busy = false;

  @override
  void dispose() {
    _sessionController.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    return ListView(
      padding: const EdgeInsets.all(24),
      children: [
        Text('终端', style: Theme.of(context).textTheme.headlineMedium),
        const SizedBox(height: 18),
        TextField(
          controller: _sessionController,
          decoration: const InputDecoration(
            labelText: 'Session ID',
            border: OutlineInputBorder(),
          ),
        ),
        const SizedBox(height: 16),
        FilledButton.icon(
          onPressed: _busy ? null : _prepareAttach,
          icon: const Icon(Icons.link_rounded),
          label: Text(_busy ? '处理中' : 'Attach'),
        ),
        const SizedBox(height: 18),
        Text(_status),
      ],
    );
  }

  Future<void> _prepareAttach() async {
    setState(() => _busy = true);
    try {
      final preview = await widget.service.prepareTerminalAttach(
        _sessionController.text.trim(),
      );
      if (!mounted) {
        return;
      }
      setState(() => _status = preview.safeMessage);
    } on NativeError catch (error) {
      if (!mounted) {
        return;
      }
      setState(() => _status = error.safeMessage);
    } finally {
      if (mounted) {
        setState(() => _busy = false);
      }
    }
  }
}
