import 'dart:js_interop';
import 'throws.dart' as app;

@JS('__dartPackDescribeError')
external set _describeError(JSFunction f);

void main() {
  // How the browser shows an uncaught Dart error: Dart's own toString.
  _describeError = ((ExternalDartReference error) => error.toDartObject.toString().toJS).toJS;
  (app.main as dynamic)();
}
