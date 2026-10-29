import 'dart:js_interop';
import 'package:web/web.dart';

class Counter {
  int clicks = 0;
  final countRef = document.querySelector('#count')!;
  final logRef = document.querySelector('#log')!;
  final itemRef = document.createElement('li');
  final incrementButtonRef = document.querySelector('#increment')!;

  void increment() {
    clicks++;
    countRef.textContent = '$clicks';
    itemRef.textContent = 'Click $clicks, handled in Dart';
    logRef.appendChild(itemRef);
  }
}


void main() {
  final counter = new Counter();
  document.querySelector('#status')!.textContent = "Dart is running. No JavaScript on this page.";

  counter.incrementButtonRef.onClick.listen((event) {
    counter.increment();
    console.log("Dart is running, as a pack in WebAssembly.".toJS);
  });
}
