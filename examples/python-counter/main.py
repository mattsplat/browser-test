# The page's globals (document, window, console, ...) come from the browser. For editor
# completion, open the repo root: Pyright/Pylance read typings/__builtins__.pyi.
document.querySelector('#status').textContent = 'Python is running, as a pack in WebAssembly.'
count = document.querySelector('#count')
log = document.querySelector('#log')
clicks = 0


def add_one(event: 'Event') -> None:  # quoted: the type exists only for the editor
    global clicks
    clicks += 1
    count.textContent = str(clicks)
    item = document.createElement('li')
    item.textContent = f'Click {clicks}, handled in Python ({event.type})'
    log.appendChild(item)


document.querySelector('#increment').addEventListener('click', add_one)
