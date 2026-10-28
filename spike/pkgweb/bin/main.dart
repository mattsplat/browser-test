import 'package:web/web.dart';

void main() {
  final status = document.querySelector('#status')!;
  status.textContent = 'Hello from package:web';
  final button = document.querySelector('#increment') as HTMLButtonElement;
  var clicks = 0;
  button.onClick.listen((event) {
    clicks++;
    final li = document.createElement('li');
    li.textContent = 'Click $clicks';
    document.querySelector('#log')!.appendChild(li);
    status.setAttribute('data-clicks', '$clicks');
  });
}
