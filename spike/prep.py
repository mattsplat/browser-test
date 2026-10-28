"""Fetch each site once and write a JS-off version: <script> removed, <noscript> shown."""
import re, subprocess, sys, pathlib

root = pathlib.Path(__file__).parent
UA = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Safari/605.1.15"

for line in (root / "sites.txt").read_text().splitlines():
    if not line.strip():
        continue
    name, url = line.split()
    # -w prints the final URL after redirects so relative links resolve correctly
    res = subprocess.run(["curl", "-sL", "--max-time", "20", "-A", UA, "-w", "\n__FINAL_URL__%{url_effective}", url],
                         capture_output=True)
    body, _, final = res.stdout.decode("utf-8", "replace").rpartition("\n__FINAL_URL__")
    scripts = len(re.findall(r"<script\b", body, re.I))
    html = re.sub(r"<script\b[^>]*>.*?</script\s*>", "", body, flags=re.I | re.S)
    html = re.sub(r"<(/?)noscript\b", r"<\1div", html, flags=re.I)
    (root / "pages" / f"{name}.html").write_text(html)
    (root / "pages" / f"{name}.url").write_text(final)
    print(f"{name:12} {len(body):>8}B  scripts_removed={scripts:<4} {final}")
