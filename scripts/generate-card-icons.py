#!/usr/bin/env python3










import argparse
import os
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
from brand import P, ellipse, circle, rrect

MUL = 'style="mix-blend-mode: multiply"'

def _m(shape_svg):
    return f'  <g {MUL}>\n  {shape_svg}  </g>\n'


def icon_svg(width, height, shapes):
    svg = f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {width} {height}" width="{width}" height="{height}">\n'
    for s in shapes:
        svg += _m(s)
    svg += '</svg>\n'
    return svg






ICONS = [


    ("session-memory", 150, 100, [
        ellipse(45, 50, 38, 30, P["pink"]),
        ellipse(85, 50, 38, 30, P["blue"]),
        circle(115, 32, 12, P["green"]),
    ]),



    ("local-first", 150, 100, [
        rrect(25, 18, 80, 22, P["green"], rx=11),
        rrect(25, 38, 80, 22, P["green"], rx=11),
        rrect(25, 58, 80, 22, P["green"], rx=11),
        circle(115, 70, 14, P["yellow"]),
    ]),



    ("multi-agent", 150, 100, [
        circle(50, 60, 30, P["blue"]),
        circle(100, 60, 30, P["green"]),
        circle(75, 30, 30, P["red"]),
    ]),



    ("hooks", 140, 100, [
        rrect(30, 8, 28, 82, P["green"], rx=14),
        ellipse(85, 50, 35, 28, P["yellow"]),
        circle(50, 78, 10, P["red"]),
    ]),



    ("swarm", 150, 100, [
        circle(50, 38, 22, P["blue"]),
        circle(90, 38, 22, P["red"]),
        circle(70, 65, 22, P["green"]),
        circle(110, 65, 16, P["yellow"]),
    ]),



    ("knowledge", 150, 100, [
        rrect(18, 25, 70, 55, P["blue"], rx=12, rotate=-6),
        rrect(35, 20, 70, 55, P["blue"], rx=12, rotate=3),
        ellipse(120, 45, 22, 18, P["yellow"]),
        circle(118, 70, 8, P["red"]),
    ]),



    ("tui", 150, 100, [
        rrect(12, 12, 110, 75, P["blue"], rx=16),
        rrect(24, 30, 40, 10, P["green"], rx=5),
        rrect(24, 48, 60, 10, P["yellow"], rx=5),
        circle(130, 20, 10, P["red"]),
    ]),



    ("web-dashboard", 150, 100, [
        rrect(10, 10, 115, 78, P["pink"], rx=18),
        rrect(22, 35, 35, 40, P["blue"], rx=8),
        rrect(65, 35, 48, 40, P["green"], rx=8),
        circle(28, 20, 5, P["red"]),
    ]),



    ("containers", 150, 100, [
        rrect(12, 12, 100, 75, P["yellow"], rx=22),
        rrect(35, 28, 55, 45, P["red"], rx=14),
        circle(120, 25, 12, P["green"]),
    ]),



    ("workflow", 150, 100, [
        rrect(8, 40, 55, 35, P["yellow"], rx=17, rotate=-10),
        rrect(52, 28, 55, 35, P["green"], rx=17),
        rrect(95, 38, 45, 30, P["red"], rx=15, rotate=8),
    ]),



    ("maintenance", 150, 100, [
        ellipse(50, 50, 35, 35, P["red"]),
        ellipse(90, 50, 30, 30, P["yellow"]),
        circle(70, 50, 14, P["green"]),
    ]),



    ("everywhere", 160, 100, [
        circle(35, 50, 28, P["blue"]),
        rrect(68, 28, 50, 44, P["pink"], rx=16),
        circle(135, 45, 22, P["green"]),
        circle(100, 75, 10, P["yellow"]),
    ]),
]


def main():
    parser = argparse.ArgumentParser(description="Generate feature card icon SVGs")
    parser.add_argument("-o", "--output-dir", default="docs_src/assets/img/cards",
                        help="Output directory")
    args = parser.parse_args()

    os.makedirs(args.output_dir, exist_ok=True)

    for slug, w, h, shapes in ICONS:
        svg = icon_svg(w, h, shapes)
        path = os.path.join(args.output_dir, f"{slug}.svg")
        with open(path, "w") as f:
            f.write(svg)
        print(f"  Written: {path}", file=sys.stderr)


if __name__ == "__main__":
    main()
