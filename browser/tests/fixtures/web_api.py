# The web platform APIs beyond the DOM, the same in every pack (web_api.dart, web_api.js).
# The cross-pack test runs all three and expects identical output.
out = document.querySelector('#out')
lines = []


def add(line):
    lines.append(line)
    out.textContent = '|'.join(lines)


# Cookies: the page's own, never HttpOnly ones (the test set one from the "server").
document.cookie = 'a=1'
document.cookie = 'b=2; Path=/'
document.cookie = 'sneaky=1; HttpOnly'
add(f'cookie:{document.cookie}')
document.cookie = 'a=; Max-Age=0'
add(f'cookie:{document.cookie}')
