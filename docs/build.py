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
from html import escape
from pathlib import Path
from string import Template

import markdown

HERE = Path(__file__).parent
OUT = HERE / "_site"

GITHUB = "https://github.com/lovelaced/unstation-desktop"
ANDROID_HELP = "https://lovelaced.github.io/unstation-android/"

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

# string.Template ($-substitution) rather than str.format: the shell embeds
# literal braces (the inline script and the speculation-rules JSON).
TEMPLATE = Template("""<!doctype html>
<html lang="en" class="no-js">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="color-scheme" content="dark">
<meta name="theme-color" content="#0B0B0E">
<title>$title</title>
<meta name="description" content="$description">
<meta property="og:site_name" content="unstation">
<meta property="og:title" content="$title">
<meta property="og:description" content="$description">
<meta property="og:type" content="website">
<link rel="icon" href="data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 16 16'%3E%3Ccircle cx='8' cy='8' r='5' fill='%23FF5C7A'/%3E%3C/svg%3E">
<link rel="preload" href="assets/fonts/saira-700.woff2" as="font" type="font/woff2" crossorigin>
<link rel="stylesheet" href="assets/css/docs.css">
<script>document.documentElement.className="js";try{if(!sessionStorage.uSeen){document.documentElement.className+=" entrance";sessionStorage.uSeen="1"}}catch(e){}</script>
<script defer src="assets/js/docs.js"></script>
<script type="speculationrules">{"prerender":[{"where":{"href_matches":"/*"},"eagerness":"moderate"}]}</script>
</head>
<body>
<a class="skip" href="#main">Skip to content</a>
<header class="site-head"><div class="wrap head-inner">
<a class="wordmark" href="index.html"><span class="dot" aria-hidden="true"></span>unstation<span class="wm-tag">docs</span></a>
<nav class="site-nav" aria-label="Docs">
<span class="nav-pill" aria-hidden="true"></span>
$nav
</nav>
<a class="gh" href="$github" rel="noopener">GitHub<span class="ext" aria-hidden="true">&#8599;</span></a>
<button class="menu-btn" type="button" aria-expanded="false" aria-controls="site-menu"><span class="menu-icon" aria-hidden="true"><span></span><span></span></span><span class="sr-only">Menu</span></button>
</div>
<div class="progress" aria-hidden="true"><span class="progress-bar"></span><span class="progress-head"></span></div>
</header>
<div class="menu-scrim" aria-hidden="true"></div>
<nav id="site-menu" class="site-menu" aria-label="Pages" inert><div class="wrap">
$menu
<div class="m-foot">
<a href="$github" rel="noopener">Source on GitHub <span class="ext" aria-hidden="true">&#8599;</span></a>
<a href="$android_help">Android app help</a>
</div>
</div></nav>
<main id="main" class="wrap doc">
$content
$pagination
</main>
$toc
<footer class="site-foot"><div class="wrap">
<p class="foot-brand"><span class="dot" aria-hidden="true"></span>unstation</p>
<p>Experimental, unaudited, and community-run. Free software under the AGPL-3.0.</p>
<p>No servers, no accounts, no one to take it down.</p>
<p class="foot-links"><a href="$github" rel="noopener">Source on GitHub</a> &middot; <a href="$android_help">Android app help</a></p>
</div></footer>
</body>
</html>
""")

FRONT_MATTER = re.compile(r"\A---\n.*?\n---\n", re.DOTALL)
# [text](page.md) or [text](page.md#anchor) -> .html, leaving external and same-page links alone.
MD_LINK = re.compile(r"\]\((?!https?://)([a-z0-9-]+)\.md(#[a-z0-9-]+)?\)")


def render(md_text):
    """Returns (content html, flat list of h2 toc tokens)."""
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
    return html, flat_h2(md.toc_tokens)


def flat_h2(tokens):
    out = []
    for t in tokens:
        if t["level"] == 2:
            out.append(t)
        out.extend(flat_h2(t.get("children", [])))
    return out


def nav_for(current):
    out = []
    for _, href, label, _ in PAGES:
        cur = ' aria-current="page"' if href == current else ""
        out.append(f'<a href="{href}"{cur}>{label}</a>')
    return "\n".join(out)


def menu_for(current):
    """The mobile sheet: each page with its one-line description."""
    out = []
    for _, href, label, desc in PAGES:
        cur = ' aria-current="page"' if href == current else ""
        out.append(f'<a class="m-link" href="{href}"{cur}>'
                   f'<span class="m-label">{label}</span>'
                   f'<span class="m-desc">{desc}</span></a>')
    return "\n".join(out)


def toc_for(h2s):
    """The "On this page" rail (wide screens only; docs.js drives the marker)."""
    if len(h2s) < 2:
        return ""
    links = "\n".join(f'<a href="#{t["id"]}">{escape(t["name"])}</a>' for t in h2s)
    return ('<aside class="toc" aria-label="On this page">\n'
            '<p class="toc-label">On this page</p>\n'
            '<nav class="toc-list"><span class="toc-marker" aria-hidden="true"></span>\n'
            f"{links}\n</nav>\n</aside>")


def pagination_for(i):
    """Previous/next cards in PAGES order."""
    parts = []
    if i > 0:
        _, href, label, _ = PAGES[i - 1]
        parts.append(f'<a class="page-link prev" href="{href}">'
                     f'<span class="k"><span class="arrow" aria-hidden="true">&#8592;</span> Previous</span>'
                     f'<span class="t">{label}</span></a>')
    if i < len(PAGES) - 1:
        _, href, label, _ = PAGES[i + 1]
        parts.append(f'<a class="page-link next" href="{href}">'
                     f'<span class="k">Next <span class="arrow" aria-hidden="true">&#8594;</span></span>'
                     f'<span class="t">{label}</span></a>')
    if not parts:
        return ""
    return '<nav class="pagination" aria-label="Adjacent pages">' + "\n".join(parts) + "</nav>"


def main():
    if OUT.exists():
        shutil.rmtree(OUT)
    OUT.mkdir(parents=True)
    shutil.copytree(HERE / "assets", OUT / "assets")

    for i, (src, href, label, desc) in enumerate(PAGES):
        content, h2s = render((HERE / src).read_text())
        title = "unstation" if href == "index.html" else f"{label} · unstation"
        (OUT / href).write_text(TEMPLATE.substitute(
            title=title, description=desc, github=GITHUB, android_help=ANDROID_HELP,
            nav=nav_for(href), menu=menu_for(href), content=content,
            toc=toc_for(h2s), pagination=pagination_for(i),
        ))
        print("built", href)
    print("done ->", OUT.relative_to(Path.cwd()) if OUT.is_relative_to(Path.cwd()) else OUT)


if __name__ == "__main__":
    main()
