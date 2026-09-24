#!/usr/bin/env python3
"""Render landing-page sections into a standalone file for visual review.

The --mk-* tokens are scoped to `.marketing-shell`, not `:root`, and the
markup leans on Tailwind utilities, so the preview wears the shell class and
loads the compiled stylesheet rather than the source partial. Sections are
lifted whole and kept inside the real wrapper, because separator offsets are
measured against its space-y step.

Usage:  python3 scripts/preview-marketing.py <out.html>
"""

import re
import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
CSS = REPO / "static/css/marketing.css"
TEMPLATE = REPO / "templates/marketing/landing.html"
WRAPPER = '<div class="space-y-24 sm:space-y-28">'

# Opening tag of each section to lift, in page order.
SECTIONS = [
    '<section id="features"',
    '<section id="checks"',
    '<section class="space-y-8">',
    '<section id="pricing"',
    '<section id="faq"',
]

SCALARS = {
    "{{ app_url }}": "#",
}

FAQ_STUB = [
    ("Is it really free?", "Yes. The Standard tier is free forever, no card."),
    ("Can I self-host it?", "Yes, it is AGPL. Self-hosting has no limits."),
]


def lift(html: str, opener: str) -> str:
    i = html.index(opener)
    j = html.index("\n    </section>", i) + len("\n    </section>")
    return html[i:j]


def render_faq_loop(fragment: str) -> str:
    """Unroll `{% for faq in faqs %}…{% endfor %}` against stub content."""
    m = re.search(r"\{%\s*for faq in faqs\s*%\}(.*?)\{%\s*endfor\s*%\}", fragment, re.S)
    if not m:
        return fragment
    body = m.group(1)
    rendered = "".join(
        body.replace("{{ faq.0 }}", q).replace("{{ faq.1|safe }}", a)
        for q, a in FAQ_STUB
    )
    return fragment[: m.start()] + rendered + fragment[m.end() :]


def clean(fragment: str) -> str:
    fragment = render_faq_loop(fragment)
    fragment = re.sub(r"\{#.*?#\}", "", fragment, flags=re.S)
    for token, value in SCALARS.items():
        fragment = fragment.replace(token, value)
    return fragment


def main() -> int:
    if len(sys.argv) < 2:
        print(__doc__.strip().splitlines()[-1], file=sys.stderr)
        return 2
    out_path = Path(sys.argv[1])
    if not CSS.exists():
        print(f"missing {CSS}; run a cargo build first", file=sys.stderr)
        return 1

    html = TEMPLATE.read_text()
    body = "\n".join(clean(lift(html, opener)) for opener in SECTIONS)

    out_path.write_text(f"""<!doctype html>
<meta charset="utf-8">
<title>landing sections</title>
<link rel="stylesheet" href="{CSS}">
<body class="marketing-shell antialiased">
<main class="container mx-auto px-4 pt-8 pb-16 sm:pt-10 sm:pb-24">
{WRAPPER}
{body}
</div>
</main>
""")

    stray = re.findall(r"\{\{.*?\}\}|\{%.*?%\}", out_path.read_text())
    if stray:
        print(f"warning: unrendered template tags: {sorted(set(stray))}", file=sys.stderr)

    print(f"wrote {out_path}")
    subprocess.run(["open", str(out_path)], check=False)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
