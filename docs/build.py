#!/usr/bin/env python3
"""Static builder for the unstation docs site. No Jekyll.

Renders each markdown page into one small styled HTML shell, rewrites the
between-page `.md` links to `.html`, and writes flat files into `_site/`. The
output uses only relative links, so it works unchanged at a project path
(https://user.github.io/repo/) or at a custom domain root.

Preview locally:
    pip install markdown
    python3 docs/build.py
    open docs/_site/index.html

The GitHub Pages workflow (.github/workflows/pages.yml) runs the same two steps
and uploads `_site/`.
"""

import re
import shutil
from pathlib import Path

import markdown

HERE = Path(__file__).parent
OUT = HERE / "_site"

# The site, in nav order. First entry is the home page.
# (source markdown, output file, nav label, meta description)
PAGES = [
    ("index.md", "index.html", "Overview",
     "Watch and broadcast live video peer to peer, with no server in the middle and no one to take it down."),
    ("how-it-works.md", "how-it-works.html", "How it works",
     "The whole system in plain but technical language, with a diagram."),
    ("protocol.md", "protocol.html", "Protocol",
     "Wire formats, the chain layer, the mesh protocol, and the trust chain."),
    ("engineering.md", "engineering.html", "Engineering",
     "Design rationale for the mesh and the SCALE wire format: the tradeoffs, why decisions were taken, and what they enable."),
    ("security.md", "security.html", "Security",
     "The honest threat model: what Unstation protects, what it doesn't, and what each party can see."),
    ("faq.md", "faq.html", "FAQ",
     "Plain answers about privacy, cost, safety, and what you need to use Unstation."),
    ("run-a-relay.md", "run-a-relay.html", "Run a relay",
     "Lend bandwidth from a spare server and help carry streams that recruit it."),
    ("contributing.md", "contributing.html", "Contributing",
     "Repo layout, building the app and the relay, and running the tests."),
]

TEMPLATE = """<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
<meta name="description" content="{description}">
<link rel="stylesheet" href="assets/css/docs.css">
</head>
<body>
<a class="skip" href="#main">Skip to content</a>
<header class="site-head"><div class="wrap head-inner">
<a class="wordmark" href="index.html">unstation</a>
<nav class="site-nav" aria-label="Docs">
{nav}
</nav>
<a class="gh" href="https://github.com/lovelaced/unstation-desktop" rel="noopener">GitHub</a>
</div></header>
<main id="main" class="wrap doc">
{content}
</main>
<footer class="site-foot"><div class="wrap">
<p>Experimental, unaudited, and community-run. Free software under the AGPL-3.0.</p>
<p>No servers, no accounts, no one to take it down.</p>
</div></footer>
</body>
</html>
"""

FRONT_MATTER = re.compile(r"\A---\n.*?\n---\n", re.DOTALL)
# [text](page.md) or [text](page.md#anchor) -> .html, leaving external and same-page links alone.
MD_LINK = re.compile(r"\]\((?!https?://)([a-z0-9-]+)\.md(#[a-z0-9-]+)?\)")


def render(md_text):
    md_text = FRONT_MATTER.sub("", md_text)            # tolerate leftover front matter
    md_text = MD_LINK.sub(r"](\1.html\2)", md_text)    # between-page links
    # `permalink` adds a small "#" link on each heading so any section (a specific FAQ
    # answer, say) can be linked to directly; the CSS reveals it on hover.
    md = markdown.Markdown(
        extensions=["tables", "fenced_code", "toc"],
        extension_configs={"toc": {"permalink": "#"}},
    )
    html = md.convert(md_text)
    # Let wide tables scroll inside their own box instead of overflowing the page on
    # narrow screens (the CSS styles .table-scroll).
    html = html.replace("<table>", '<div class="table-scroll"><table>').replace(
        "</table>", "</table></div>")
    return html


def nav_for(current):
    out = []
    for _, href, label, _ in PAGES:
        cur = ' aria-current="page"' if href == current else ""
        out.append(f'<a href="{href}"{cur}>{label}</a>')
    return "\n".join(out)


def main():
    if OUT.exists():
        shutil.rmtree(OUT)
    OUT.mkdir(parents=True)
    shutil.copytree(HERE / "assets", OUT / "assets")

    for src, href, label, desc in PAGES:
        content = render((HERE / src).read_text())
        title = "unstation" if href == "index.html" else f"{label} · unstation"
        (OUT / href).write_text(
            TEMPLATE.format(title=title, description=desc, nav=nav_for(href), content=content)
        )
        print("built", href)
    print("done ->", OUT.relative_to(Path.cwd()) if OUT.is_relative_to(Path.cwd()) else OUT)


if __name__ == "__main__":
    main()
