import '../../packs/dart/dom.dart';

void main() {
  query('#status')?.text = 'Dart is running. No JavaScript on this page.';

  final count = query('#count')!;
  final log = query('#log')!;
  var clicks = 0;

  query('#increment')!.onClick(() {
    clicks++;
    count.text = '$clicks';
    log.append(create('li')..text = 'Click $clicks, handled in Dart');
  });
}
