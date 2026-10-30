# A to-do list kept in localStorage. Python here has no json module, so the list is saved as
# one line per item: "1" or "0" for done, then the text (which can't hold a newline, since it
# comes from a one-line input).

KEY = 'todo-items'
items = []           # [{'text': str, 'done': bool}]
view = 'all'         # 'all', 'active' or 'done'


def load():
    saved = localStorage.getItem(KEY)
    if saved:
        for line in saved.split('\n'):
            if line:
                items.append({'text': line[1:], 'done': line[0] == '1'})


def save():
    localStorage.setItem(KEY, '\n'.join(('1' if item['done'] else '0') + item['text'] for item in items))


def visible(item):
    return view == 'all' or (view == 'done') == item['done']


def render():
    listing = document.querySelector('#items')
    listing.textContent = ''
    for index, item in enumerate(items):
        if not visible(item):
            continue
        row = document.createElement('li')
        if item['done']:
            row.className = 'done'
        tick = document.createElement('input')
        tick.setAttribute('type', 'checkbox')
        tick.checked = item['done']
        tick.addEventListener('change', lambda event, i=index: toggle(i))
        text = document.createElement('span')
        text.textContent = item['text']
        delete = document.createElement('button')
        delete.className = 'delete'
        delete.textContent = '×'
        delete.addEventListener('click', lambda event, i=index: remove(i))
        row.appendChild(tick)
        row.appendChild(text)
        row.appendChild(delete)
        listing.appendChild(row)
    left = sum(1 for item in items if not item['done'])
    document.querySelector('#left').textContent = f'{left} item{"" if left == 1 else "s"} left'


def add(text):
    text = text.strip()
    if text:
        items.append({'text': text, 'done': False})
        save()
        render()


def toggle(index):
    items[index]['done'] = not items[index]['done']
    save()
    render()


def remove(index):
    del items[index]
    save()
    render()


def on_submit(event):
    event.preventDefault()
    field = document.querySelector('#text')
    add(field.value)
    field.value = ''


def on_filter(event):
    global view
    view = event.target.dataset['filter']
    for button in document.querySelectorAll('.filter'):
        button.className = 'filter on' if button.dataset['filter'] == view else 'filter'
    render()


def on_clear_done(event):
    items[:] = [item for item in items if not item['done']]
    save()
    render()


document.querySelector('#new').addEventListener('submit', on_submit)
for button in document.querySelectorAll('.filter'):
    button.addEventListener('click', on_filter)
document.querySelector('#clear-done').addEventListener('click', on_clear_done)

load()
if not items:
    for text in ['Try the Python to-do list', 'Tick this one off', 'Reload: it stays']:
        items.append({'text': text, 'done': False})
    save()
render()
