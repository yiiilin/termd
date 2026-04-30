import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:termui_native/app/termui_native_app.dart';
import 'package:termui_native/core/services/termui_native_service.dart';

void main() {
  testWidgets('app shell exposes pairing sessions and terminal entries', (
    tester,
  ) async {
    await tester.pumpWidget(
      TermuiNativeApp(service: TermuiNativeService.productionUnsupported()),
    );

    expect(find.text('配对'), findsWidgets);
    expect(find.text('会话'), findsWidgets);
    expect(find.text('终端'), findsWidgets);

    await tester.tap(find.text('会话').last);
    await tester.pumpAndSettle();
    expect(find.byIcon(Icons.refresh_rounded), findsOneWidget);

    await tester.tap(find.text('终端').last);
    await tester.pumpAndSettle();
    expect(find.text('Session ID'), findsOneWidget);
  });
}
