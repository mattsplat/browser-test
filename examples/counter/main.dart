import 'package:web/web.dart';

void main() {
  document.querySelector('#status')!.textContent = 'Dart is running. No JavaScript on this page.';

  final count = document.querySelector('#count')!;
  final log = document.querySelector('#log')!;
  var clicks = 0;

  document.querySelector('#increment')!.onClick.listen((event) {
    clicks++;
    count.textContent = '$clicks';
    final item = document.createElement('li')..textContent = 'Click $clicks, handled in Dart (${event.type})';
    log.appendChild(item);
  });
}
