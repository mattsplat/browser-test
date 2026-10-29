// The same behaviour as events.js, through package:web. The cross-pack test runs both and
// expects identical output.
import 'dart:async';
import 'dart:js_interop';
import 'package:web/web.dart';

// Math.random, through the host like any other call.
extension type JSMath._(JSObject _) implements JSObject {
  external double random();
}

@JS('Math')
external JSMath get math;

void main() {
  final out = document.querySelector('#out')!;
  final log = <String>[];
  void add(String entry) {
    log.add(entry);
    out.textContent = log.join(',');
  }

  final button = document.querySelector('#button')!;
  // A listener that removes itself, added twice (the duplicate is ignored).
  late final JSFunction once;
  once = ((Event e) {
    add('once');
    button.removeEventListener('click', once);
  }).toJS;
  button.addEventListener('click', once);
  button.addEventListener('click', once);
  button.addEventListener('click', ((MouseEvent e) {
    add('click:${e.clientX.toInt()},${e.clientY.toInt()},${e.button},${e.shiftKey}');
  }).toJS);

  // stopPropagation keeps the click from reaching #outer.
  document.querySelector('#outer')!.addEventListener('click', ((Event e) => add('outer')).toJS);
  document.querySelector('#stop')!.addEventListener('click', ((Event e) {
    e.stopPropagation();
    add('stop');
  }).toJS);

  // Keyboard details, and preventDefault.
  document.querySelector('#field')!.addEventListener('keydown', ((KeyboardEvent e) {
    e.preventDefault();
    add('key:${e.key},${e.code},${e.ctrlKey}');
  }).toJS);

  // A Dart stream subscription, cancelled after the first event (removeEventListener inside).
  late final StreamSubscription<MouseEvent> sub;
  sub = document.querySelector('#stream')!.onClick.listen((e) {
    add('stream');
    sub.cancel();
  });

  // Timers.
  Timer(const Duration(milliseconds: 1), () => add('timeout'));
  Timer(const Duration(milliseconds: 1), () => add('cancelled!')).cancel();
  var ticks = 0;
  Timer.periodic(const Duration(milliseconds: 1), (t) {
    ticks++;
    if (ticks == 3) {
      t.cancel();
      add('interval:3');
    }
  });
  window.requestAnimationFrame(((JSNumber t) => add('frame')).toJS);

  // Nondeterminism, for the replay test: Math.random and the clock, now and from a timer.
  final noise = document.querySelector('#noise')!;
  noise.addEventListener('click', ((Event e) {
    noise.textContent = '${noise.textContent} ${(math.random() * 1e9).floor()}:${DateTime.now().millisecondsSinceEpoch}';
    Timer(const Duration(milliseconds: 1), () {
      noise.textContent = '${noise.textContent} t${(math.random() * 1e9).floor()}';
    });
  }).toJS);
}
