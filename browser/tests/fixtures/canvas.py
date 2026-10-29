# Canvas 2D, the same in every pack (canvas.dart, canvas.js). The cross-pack test runs all
# three and compares their output and their pixels.
out = document.getElementById('out')
lines = []


def add(line):
    lines.append(line)
    out.textContent = '|'.join(lines)


def js(v):
    return 'null' if v is None else str(v)


canvas = document.getElementById('c')
ctx = canvas.getContext('2d')
add(f"size:{canvas.width},{canvas.height},{js(canvas.getContext('webgl'))}")
ctx.fillStyle = 'red'
ctx.fillRect(0, 0, 20, 10)
ctx.strokeStyle = 'rgb(0, 128, 0)'
ctx.lineWidth = 4
ctx.strokeRect(25, 2, 10, 10)
ctx.fillStyle = 'blue'
ctx.beginPath()
ctx.arc(10, 25, 6, 0, 3.141592653589793 * 2)
ctx.fill()
ctx.beginPath()
ctx.moveTo(25, 20)
ctx.lineTo(39, 20)
ctx.lineTo(39, 34)
ctx.closePath()
ctx.fillStyle = '#ff00ff'
ctx.fill()
ctx.clearRect(0, 0, 5, 5)
ctx.fillStyle = 'black'
ctx.font = '14px sans-serif'
ctx.fillText('Hi', 42, 14)
add(f'style:{ctx.fillStyle},{ctx.strokeStyle},{ctx.lineWidth},{ctx.font}')
add(f"url:{'true' if canvas.toDataURL().startswith('data:image/png;base64,') else 'false'}")
