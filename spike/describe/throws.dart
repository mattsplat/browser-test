import 'package:web/web.dart';

void main() {
  print('stack: ${StackTrace.current}');
  document.querySelector('#go')!.onClick.listen((_) => explode());
}

void explode() {
  throw StateError('boom from a click');
}
