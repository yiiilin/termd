import 'package:flutter/material.dart';

import '../core/services/termui_native_service.dart';
import '../features/pairing/pairing_screen.dart';
import '../features/sessions/session_list_screen.dart';
import '../features/terminal/terminal_screen.dart';
import 'app_routes.dart';

class TermuiNativeApp extends StatelessWidget {
  const TermuiNativeApp({
    required this.service,
    super.key,
  });

  final TermuiNativeService service;

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      debugShowCheckedModeBanner: false,
      title: 'termd native',
      theme: ThemeData(
        colorScheme: ColorScheme.fromSeed(
          seedColor: const Color(0xff1f7a5c),
          brightness: Brightness.dark,
        ),
        useMaterial3: true,
      ),
      home: TermuiNativeShell(service: service),
    );
  }
}

class TermuiNativeShell extends StatefulWidget {
  const TermuiNativeShell({
    required this.service,
    super.key,
  });

  final TermuiNativeService service;

  @override
  State<TermuiNativeShell> createState() => _TermuiNativeShellState();
}

class _TermuiNativeShellState extends State<TermuiNativeShell> {
  AppRoute _selected = AppRoute.pairing;

  @override
  Widget build(BuildContext context) {
    final routeIndex = AppRoute.values.indexOf(_selected);
    final screen = _screenFor(_selected);

    return LayoutBuilder(
      builder: (context, constraints) {
        final useRail = constraints.maxWidth >= 720;

        if (!useRail) {
          return Scaffold(
            appBar: AppBar(title: const Text('termd native')),
            body: SafeArea(child: screen),
            bottomNavigationBar: NavigationBar(
              selectedIndex: routeIndex,
              onDestinationSelected: _selectIndex,
              destinations: [
                for (final route in AppRoute.values)
                  NavigationDestination(
                    icon: Icon(route.icon),
                    label: route.label,
                  ),
              ],
            ),
          );
        }

        return Scaffold(
          body: SafeArea(
            child: Row(
              children: [
                NavigationRail(
                  selectedIndex: routeIndex,
                  onDestinationSelected: _selectIndex,
                  labelType: NavigationRailLabelType.all,
                  leading: const Padding(
                    padding: EdgeInsets.symmetric(vertical: 18),
                    child: Text('termd'),
                  ),
                  destinations: [
                    for (final route in AppRoute.values)
                      NavigationRailDestination(
                        icon: Icon(route.icon),
                        label: Text(route.label),
                      ),
                  ],
                ),
                const VerticalDivider(width: 1),
                Expanded(child: screen),
              ],
            ),
          ),
        );
      },
    );
  }

  void _selectIndex(int index) {
    setState(() => _selected = AppRoute.values[index]);
  }

  Widget _screenFor(AppRoute route) {
    switch (route) {
      case AppRoute.pairing:
        return PairingScreen(service: widget.service);
      case AppRoute.sessions:
        return SessionListScreen(service: widget.service);
      case AppRoute.terminal:
        return TerminalScreen(service: widget.service);
    }
  }
}
