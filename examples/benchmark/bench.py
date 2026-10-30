# The benchmark in Python. bench.js, bench.php and bench.dart do the same work the same way,
# so the chart compares the languages, not the algorithms.

LANG = 'python'
LANGUAGES = [('js', 'JavaScript', '#D4A72C'), ('python', 'Python', '#3B82F6'), ('php', 'PHP', '#8B5CF6'), ('dart', 'Dart', '#14B8A6')]
TASKS = [
    ('fib', 'fib(25), recursive'),
    ('sieve', 'Primes below 1,000,000'),
    ('strings', 'Join and split 20,000 strings'),
    ('dom', 'Create and remove 1,000 elements'),
    ('canvas', 'Draw 3,000 rectangles'),
]


def fib(n):
    return n if n < 2 else fib(n - 1) + fib(n - 2)


# Each task returns a check value, the same in every language.
def run_fib():
    return fib(25)


def run_sieve():
    n = 1000000
    composite = bytearray(n + 1)
    count = 0
    for i in range(2, n + 1):
        if not composite[i]:
            count += 1
            for j in range(i * i, n + 1, i):
                composite[j] = 1
    return count


def run_strings():
    parts = []
    for i in range(20000):
        parts.append('item ' + str(i))
    return len(','.join(parts).split(','))


def run_dom():
    items = document.querySelector('#dom')
    for i in range(1000):
        item = document.createElement('li')
        item.textContent = 'item ' + str(i)
        items.appendChild(item)
    count = items.children.length
    items.textContent = ''
    return count


def run_canvas():
    # A Park-Miller sequence, stepped with Schrage's method so no product passes 2^31 (PHP's integers are 32-bit here): every language draws the same rectangles in the same colors.
    ctx = document.querySelector('#scratch').getContext('2d')
    seed = 1

    def step(seed):
        seed = 48271 * (seed % 44488) - 3399 * (seed // 44488)
        return seed + 2147483647 if seed <= 0 else seed

    for i in range(3000):
        seed = step(seed)
        x = seed % 300
        seed = step(seed)
        y = seed % 150
        seed = step(seed)
        c = seed
        ctx.fillStyle = f'rgb({c % 256}, {(c >> 8) % 256}, {(c >> 16) % 256})'
        ctx.fillRect(x, y, 10, 10)
    return seed


RUN = {'fib': run_fib, 'sieve': run_sieve, 'strings': run_strings, 'dom': run_dom, 'canvas': run_canvas}
results = {}


def status(text):
    document.querySelector('#status').textContent = text


def add_row(label, ms, check):
    row = document.createElement('tr')
    for text, cls in [(label, ''), (f'{ms:.1f}', 'ms'), (str(check), '')]:
        cell = document.createElement('td')
        cell.textContent = text
        if cls:
            cell.className = cls
        row.appendChild(cell)
    document.querySelector('#rows').appendChild(row)


# Each task runs in its own timer callback, so the page repaints between them and no single
# callback comes near the 5 s limit.
def step(i):
    if i == len(TASKS):
        localStorage.setItem('bench:' + LANG, ';'.join(f'{key}={results[key]:.2f}' for key, _ in TASKS))
        status('Done. Run the other languages to compare them.')
        draw_chart()
        return
    key, label = TASKS[i]
    status(f'Running: {label}…')

    def run():
        start = performance.now()
        check = RUN[key]()
        ms = performance.now() - start
        results[key] = ms
        add_row(label, ms, check)
        step(i + 1)

    setTimeout(run, 0)


def saved_results():
    """Every language's saved results, as {lang: {task: ms}}."""
    all_results = {}
    for code, _, _ in LANGUAGES:
        saved = localStorage.getItem('bench:' + code)
        if not saved:
            continue
        all_results[code] = {}
        for pair in saved.split(';'):
            key, ms = pair.split('=')
            all_results[code][key] = float(ms)
    return all_results


def draw_chart():
    """One group of bars per task, one bar per language; the slowest fills the width."""
    ctx = document.querySelector('#chart').getContext('2d')
    all_results = saved_results()
    ctx.fillStyle = '#ffffff'
    ctx.fillRect(0, 0, 640, 380)
    ctx.font = '12px sans-serif'
    y = 16
    for key, label in TASKS:
        ctx.fillStyle = '#222222'
        ctx.fillText(label, 8, y)
        y += 6
        biggest = max([1e-9] + [r.get(key, 0) for r in all_results.values()])
        for code, name, color in LANGUAGES:
            if code not in all_results:
                continue
            ms = all_results[code][key]
            width = max(1, ms / biggest * 440)
            ctx.fillStyle = color
            ctx.fillRect(90, y, width, 11)
            ctx.fillStyle = '#444444'
            ctx.fillText(name, 8, y + 10)
            ctx.fillText(f'{ms:.1f} ms', 96 + width, y + 10)
            y += 13
        y += 14
    if not all_results:
        ctx.fillStyle = '#888888'
        ctx.fillText('No results yet: press Run.', 8, y)


def on_run(event):
    results.clear()
    document.querySelector('#rows').textContent = ''
    step(0)


def on_clear(event):
    for code, _, _ in LANGUAGES:
        localStorage.removeItem('bench:' + code)
    document.querySelector('#rows').textContent = ''
    draw_chart()


document.querySelector('#run').addEventListener('click', on_run)
document.querySelector('#clear').addEventListener('click', on_clear)
status('Python is ready.')
draw_chart()
