# IndexedDB, the same in every pack (idb.dart, idb.js). The cross-pack test runs all three
# and expects identical output.
out = document.getElementById('out')
lines = []


def add(line):
    lines.append(line)
    out.textContent = '|'.join(lines)


def js(v):
    return ('true' if v else 'false') if isinstance(v, bool) else str(v)


request = indexedDB.open('shop', 2)


def on_upgrade(e):
    db = request.result
    add(f'upgrade:{db.version},{db.objectStoreNames.length}')
    db.createObjectStore('items', {'keyPath': 'id', 'autoIncrement': True})
    db.createObjectStore('notes')


def on_open(e):
    db = request.result
    add(f"open:{db.name},{db.version},{'+'.join(db.objectStoreNames)}")
    tx = db.transaction(['items', 'notes'], 'readwrite')
    items = tx.objectStore('items')
    # The secret is stored and read back, never shown: AI views must not see it.
    items.put({'name': 'apple', 'tags': ['red', 'fruit'], 'secret': 'S3CRET-IDB'}).onsuccess = lambda e: add(f'put:{e.target.result}')
    items.put({'id': 10, 'name': 'pear', 'n': 2.5, 'ok': True, 'none': None})
    items.add({'id': 10, 'name': 'dup'}).onerror = lambda e: add(f'add:{e.target.error.name}')
    tx.objectStore('notes').put('hello', 'greeting')

    def on_complete(e):
        add('complete')
        store = db.transaction('items').objectStore('items')
        add(f'store:{store.keyPath},{js(store.autoIncrement)}')

        def on_get(e):
            v = e.target.result
            add(f'get:{v.name},{len(v.tags)},{v.tags[1]},{len(v.secret)}')
        store.get(1).onsuccess = on_get
        store.get(99).onsuccess = lambda e: add(f'missing:{js(e.target.result is None)}')
        store.getAll().onsuccess = lambda e: add(f"all:{'+'.join(v.name for v in e.target.result)}")
        counted = store.count()

        def on_count(e):
            add(f'count:{counted.result}')
            write = db.transaction('items', 'readwrite')
            s = write.objectStore('items')
            s.delete(10)
            s.count().onsuccess = lambda e: add(f'after-delete:{e.target.result}')
            s.clear()
            s.count().onsuccess = lambda e: add(f'cleared:{e.target.result}')
            write.addEventListener('complete', lambda e: add('done'))
        counted.onsuccess = on_count
    tx.oncomplete = on_complete


request.onupgradeneeded = on_upgrade
request.onsuccess = on_open
