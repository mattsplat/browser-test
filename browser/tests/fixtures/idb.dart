// IndexedDB, the same in every pack (idb.js, idb.py), through package:web. The cross-pack
// test runs all three and expects identical output.
import 'dart:js_interop';
import 'dart:js_interop_unsafe';
import 'package:web/web.dart';

void main() {
  final out = document.getElementById('out')!;
  final lines = <String>[];
  void add(String line) {
    lines.add(line);
    out.textContent = lines.join('|');
  }

  String field(JSAny? o, String name) => (o as JSObject).getProperty(name.toJS).toString();
  int number(JSAny? n) => (n as JSNumber).toDartInt;
  JSAny? result(Event e) => (e.target as IDBRequest).result;

  final request = window.indexedDB.open('shop', 2);
  request.onupgradeneeded = ((Event e) {
    final db = request.result as IDBDatabase;
    add('upgrade:${db.version},${db.objectStoreNames.length}');
    db.createObjectStore('items', IDBObjectStoreParameters(keyPath: 'id'.toJS, autoIncrement: true));
    db.createObjectStore('notes');
  }).toJS;
  request.onsuccess = ((Event e) {
    final db = request.result as IDBDatabase;
    final names = db.objectStoreNames as JSArray<JSString>;
    add('open:${db.name},${db.version},${names.toDart.map((n) => n.toDart).join('+')}');
    final tx = db.transaction(['items'.toJS, 'notes'.toJS].toJS, 'readwrite');
    final items = tx.objectStore('items');
    // The secret is stored and read back, never shown: AI views must not see it.
    items.put({'name': 'apple', 'tags': ['red', 'fruit'], 'secret': 'S3CRET-IDB'}.jsify()).onsuccess = ((Event e) => add('put:${number(result(e))}')).toJS;
    items.put({'id': 10, 'name': 'pear', 'n': 2.5, 'ok': true, 'none': null}.jsify());
    items.add({'id': 10, 'name': 'dup'}.jsify()).onerror = ((Event e) => add('add:${(e.target as IDBRequest).error!.name}')).toJS;
    tx.objectStore('notes').put('hello'.toJS, 'greeting'.toJS);
    tx.oncomplete = ((Event e) {
      add('complete');
      final store = db.transaction('items'.toJS).objectStore('items');
      add('store:${(store.keyPath as JSString).toDart},${store.autoIncrement}');
      store.get(1.toJS).onsuccess = ((Event e) {
        final v = result(e);
        final tags = (v as JSObject).getProperty<JSArray<JSString>>('tags'.toJS).toDart;
        add('get:${field(v, 'name')},${tags.length},${tags[1].toDart},${field(v, 'secret').length}');
      }).toJS;
      store.get(99.toJS).onsuccess = ((Event e) => add('missing:${result(e) == null}')).toJS;
      store.getAll().onsuccess = ((Event e) {
        final all = (result(e) as JSArray<JSObject>).toDart;
        add('all:${all.map((v) => field(v, 'name')).join('+')}');
      }).toJS;
      final counted = store.count();
      counted.onsuccess = ((Event e) {
        add('count:${number(counted.result)}');
        final write = db.transaction('items'.toJS, 'readwrite');
        final s = write.objectStore('items');
        s.delete(10.toJS);
        s.count().onsuccess = ((Event e) => add('after-delete:${number(result(e))}')).toJS;
        s.clear();
        s.count().onsuccess = ((Event e) => add('cleared:${number(result(e))}')).toJS;
        write.addEventListener('complete', ((Event e) => add('done')).toJS);
      }).toJS;
    }).toJS;
  }).toJS;
}
