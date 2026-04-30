import 'package:flutter/material.dart';

import '../../core/errors/native_error.dart';
import '../../core/protocol/protocol_types.dart';
import '../../core/services/termui_native_service.dart';

class SessionListScreen extends StatefulWidget {
  const SessionListScreen({
    required this.service,
    super.key,
  });

  final TermuiNativeService service;

  @override
  State<SessionListScreen> createState() => _SessionListScreenState();
}

class _SessionListScreenState extends State<SessionListScreen> {
  late Future<WorkbenchSnapshot> _snapshot;
  Future<List<RemoteSessionSummary>>? _sessions;
  String? _sessionError;

  @override
  void initState() {
    super.initState();
    _snapshot = widget.service.loadWorkbenchSnapshot();
  }

  @override
  Widget build(BuildContext context) {
    return RefreshIndicator(
      onRefresh: _refresh,
      child: ListView(
        padding: const EdgeInsets.all(24),
        children: [
          Text('会话', style: Theme.of(context).textTheme.headlineMedium),
          const SizedBox(height: 18),
          FutureBuilder<WorkbenchSnapshot>(
            future: _snapshot,
            builder: (context, snapshot) {
              final data = snapshot.data;
              if (data == null) {
                return const LinearProgressIndicator();
              }
              return Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  Text('本机状态：${data.statusMessage}'),
                  const SizedBox(height: 8),
                  Text('已配对 daemon：${data.pairedServers.length}'),
                ],
              );
            },
          ),
          const SizedBox(height: 20),
          FilledButton.icon(
            onPressed: _loadSessions,
            icon: const Icon(Icons.refresh_rounded),
            label: const Text('刷新会话'),
          ),
          const SizedBox(height: 16),
          if (_sessionError != null) Text(_sessionError!),
          if (_sessions != null)
            FutureBuilder<List<RemoteSessionSummary>>(
              future: _sessions,
              builder: (context, snapshot) {
                if (snapshot.connectionState != ConnectionState.done) {
                  return const LinearProgressIndicator();
                }
                final sessions = snapshot.data ?? const [];
                if (sessions.isEmpty) {
                  return const Text('暂无可展示会话');
                }
                return Column(
                  children: [
                    for (final session in sessions)
                      ListTile(
                        leading: const Icon(Icons.terminal_rounded),
                        title: Text(session.sessionId),
                        subtitle: Text(session.state.wireName),
                      ),
                  ],
                );
              },
            ),
        ],
      ),
    );
  }

  Future<void> _refresh() async {
    setState(() {
      _snapshot = widget.service.loadWorkbenchSnapshot();
      _sessionError = null;
    });
    await _snapshot;
  }

  void _loadSessions() {
    setState(() {
      _sessionError = null;
      _sessions = widget.service.listSessions().catchError((Object error) {
        if (error is NativeError) {
          _sessionError = error.safeMessage;
        } else {
          _sessionError = '会话列表不可用';
        }
        return const <RemoteSessionSummary>[];
      });
    });
  }
}
