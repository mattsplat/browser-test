// IndexedDB, the same in every pack (idb.dart, idb.py). The cross-pack test runs all three
// and expects identical output.
const out = document.getElementById('out');
const lines = [];
const add = (line) => {
  lines.push(line);
  out.textContent = lines.join('|');
};

const request = indexedDB.open('shop', 2);
request.onupgradeneeded = () => {
  const db = request.result;
  add(`upgrade:${db.version},${db.objectStoreNames.length}`);
  db.createObjectStore('items', { keyPath: 'id', autoIncrement: true });
  db.createObjectStore('notes');
};
request.onsuccess = () => {
  const db = request.result;
  add(`open:${db.name},${db.version},${[...db.objectStoreNames].join('+')}`);
  const tx = db.transaction(['items', 'notes'], 'readwrite');
  const items = tx.objectStore('items');
  // The secret is stored and read back, never shown: AI views must not see it.
  items.put({ name: 'apple', tags: ['red', 'fruit'], secret: 'S3CRET-IDB' }).onsuccess = (e) => add(`put:${e.target.result}`);
  items.put({ id: 10, name: 'pear', n: 2.5, ok: true, none: null });
  items.add({ id: 10, name: 'dup' }).onerror = (e) => add(`add:${e.target.error.name}`);
  tx.objectStore('notes').put('hello', 'greeting');
  tx.oncomplete = () => {
    add('complete');
    const store = db.transaction('items').objectStore('items');
    add(`store:${store.keyPath},${store.autoIncrement}`);
    store.get(1).onsuccess = (e) => {
      const v = e.target.result;
      add(`get:${v.name},${v.tags.length},${v.tags[1]},${v.secret.length}`);
    };
    store.get(99).onsuccess = (e) => add(`missing:${e.target.result === undefined}`);
    store.getAll().onsuccess = (e) => add(`all:${[...e.target.result].map((v) => v.name).join('+')}`);
    const counted = store.count();
    counted.onsuccess = () => {
      add(`count:${counted.result}`);
      const write = db.transaction('items', 'readwrite');
      const s = write.objectStore('items');
      s.delete(10);
      s.count().onsuccess = (e) => add(`after-delete:${e.target.result}`);
      s.clear();
      s.count().onsuccess = (e) => add(`cleared:${e.target.result}`);
      write.addEventListener('complete', () => add('done'));
    };
  };
};
