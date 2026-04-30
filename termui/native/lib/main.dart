import 'package:flutter/material.dart';

import 'app/termui_native_app.dart';
import 'core/services/termui_native_service.dart';

void main() {
  WidgetsFlutterBinding.ensureInitialized();

  runApp(
    TermuiNativeApp(
      service: TermuiNativeService.productionUnsupported(),
    ),
  );
}
