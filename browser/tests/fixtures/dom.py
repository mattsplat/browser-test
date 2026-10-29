# DOM breadth and form events, the same in every pack (dom.dart, dom.js). The cross-pack
# test runs all three and expects identical output.
out = document.getElementById('out')
lines = []


def add(line):
    lines.append(line)
    out.textContent = '|'.join(lines)


def js(v):
    """A value as JS would print it."""
    if v is None:
        return 'null'
    if isinstance(v, bool):
        return 'true' if v else 'false'
    return str(v)


# Queries: a NodeList by index, item() and iteration.
all = document.querySelectorAll('#root p')
add(f"query:{len(all)},{all.item(0).id},{all[1].id},{js(all.item(9))},{'+'.join(p.textContent for p in all)},{js(document.getElementById('nope'))}")

# The tree.
root = document.getElementById('root')
p1 = document.getElementById('p1')
p2 = document.getElementById('p2')
s = document.getElementById('s')
add(f'tree:{root.children.length},{root.children.item(2).tagName},{p1.parentElement.id},{root.firstChild.nextSibling.id},{s.previousSibling.id},{root.lastChild.textContent},{p1.firstChild.nodeType},{root.parentElement.tagName}')

# Moving, replacing, removing and cloning nodes.
root.insertBefore(s, p1)
em = document.createElement('em')
em.textContent = 'new'
replaced = root.replaceChild(em, p2)
root.removeChild(s)
copy = p1.cloneNode(True)
copy.id = 'p3'
root.appendChild(copy)
shallow = p1.cloneNode(False)
add(f"mutate:{'+'.join(c.id or c.tagName for c in root.children)},{replaced.id},{shallow.childNodes.length},{copy.textContent},{js(root.contains(copy))},{js(root.contains(s))}")

# classList, dataset and style.
classes = p1.classList
classes.add('c')
classes.remove('a')
add(f"class:{js(classes.toggle('b'))},{js(classes.toggle('d', True))},{js(classes.contains('c'))},{js(classes.contains('a'))},{classes.length},{p1.className}")
p1.dataset.fooBar = 'x'
add(f"data:{p1.dataset['userId']},{p1.getAttribute('data-foo-bar')}")
p1.style.setProperty('background-color', 'blue')
p1.style.marginTop = '4px'
css = p1.style.cssText
color = p1.style.getPropertyValue('color')
removed = p1.style.removeProperty('color')
after = p1.style.color
p1.style.cssText = 'width: 10px'
add(f"style:{css},{color},{removed},{after},{p1.style.width},{p1.getAttribute('style')}")

# innerHTML is parsed; the script in it never runs. outerHTML serialises.
root.innerHTML = '<b class="x">bold &amp; <i>it</i></b><script>document.getElementById("out").textContent = "RAN"</script>'
add(f'html:{root.children.length},{root.innerHTML},{root.firstChild.outerHTML}')

# Form fields: value, checked, disabled.
name = document.getElementById('name')
agree = document.getElementById('agree')
off = document.getElementById('off')
initial = name.value
name.value = 'typed'
agree.checked = True
was_disabled = off.disabled
off.disabled = False
add(f"form:{initial},{name.value},{name.getAttribute('value')},{js(was_disabled)},{js(agree.checked)},{js(off.disabled)},{js(off.hasAttribute('disabled'))}")

# A password's value is read, never shown: AI views must not see it.
pw = document.getElementById('pw')
add(f"secret:{len(pw.value)},{js(len(document.getElementById('form').innerHTML) > 0)}")

# Form events: focus, input, change, blur and submit (cancelled).
name.addEventListener('focus', lambda e: add('focus'))
name.addEventListener('blur', lambda e: add('blur'))
name.addEventListener('input', lambda e: add(f'input:{e.target.value}'))
name.addEventListener('change', lambda e: add(f'change:{name.value}'))
agree.addEventListener('change', lambda e: add(f'toggle:{js(agree.checked)}'))


def on_submit(e):
    e.preventDefault()
    add('submit')


document.getElementById('form').addEventListener('submit', on_submit)

# The box's layout, once laid out.
box = document.getElementById('box')


def on_box(e):
    r = box.getBoundingClientRect()
    add(f'rect:{r.x},{r.y},{r.width},{r.height},{r.right},{r.bottom}')


box.addEventListener('click', on_box)
name.focus()
