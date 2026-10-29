import 'package:web/web.dart';

// A click runs forever; the browser's time limit must stop it.
void main() {
  document.querySelector('#go')!.onClick.listen((_) {
    var n = 0;
    while (true) {
      n++;
    }
  });
}
