import 'package:flutter/material.dart';

/// Native 骨架只提供三类工作区入口；协议流程和权限判断必须留在 service/daemon 边界。
enum AppRoute {
  pairing('/pairing', '配对', Icons.key_rounded),
  sessions('/sessions', '会话', Icons.list_alt_rounded),
  terminal('/terminal', '终端', Icons.terminal_rounded);

  const AppRoute(this.path, this.label, this.icon);

  final String path;
  final String label;
  final IconData icon;
}
