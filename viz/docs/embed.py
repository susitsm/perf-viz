#!/usr/bin/env python3
"""Turns docs/overview.src.md into docs/overview.md, with the images inlined as data URIs.

rustdoc does not copy images next to the generated pages, so the pictures travel inside the
markdown that main.rs includes as its module documentation.
"""
import base64, os, re, sys

here = os.path.dirname(os.path.abspath(__file__))
src = open(os.path.join(here, "overview.src.md")).read()

def inline(m):
    alt, path = m.group(1), m.group(2)
    data = base64.b64encode(open(os.path.join(here, path), "rb").read()).decode()
    return f'<img alt="{alt}" src="data:image/png;base64,{data}" style="max-width:100%">'

out = re.sub(r"!\[([^\]]*)\]\((images/[^)]+)\)", inline, src)
open(os.path.join(here, "overview.md"), "w").write(out)
print("overview.md:", len(out) // 1024, "KiB")
