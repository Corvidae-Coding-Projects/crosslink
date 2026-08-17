#!/usr/bin/env python3
















import argparse
import random
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
from brand import P, CONFETTI_COLORS, ellipse, circle, rrect, confetti, write_svg

WIDTH = 1500
HEIGHT = 500
SEED = 42

MUL = 'style="mix-blend-mode: multiply"'

def _m(shape_svg):

    return f'  <g {MUL}>\n  {shape_svg}  </g>\n'


def generate():
    rng = random.Random(SEED)

    svg = f"""<?xml version="1.0" encoding="UTF-8"?>
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {WIDTH} {HEIGHT}" width="{WIDTH}" height="{HEIGHT}">
  <rect width="{WIDTH}" height="{HEIGHT}" fill="{P['bg']}"/>
"""


    svg += _m(ellipse(820, 260, 260, 200, P["pink"]))
    svg += _m(rrect(1050, 50, 320, 160, P["yellow"], rx=70, rotate=8))
    svg += _m(ellipse(200, 310, 200, 160, P["blue"]))
    svg += _m(ellipse(1380, 220, 150, 130, P["pink"]))
    svg += _m(ellipse(60, 140, 120, 100, P["yellow"]))


    svg += _m(circle(260, 190, 90, P["green"]))
    svg += _m(circle(320, 125, 18, P["red"]))

    svg += _m(ellipse(530, 280, 100, 75, P["pink"]))

    svg += _m(rrect(700, 150, 200, 220, P["green"], rx=60))
    svg += _m(circle(870, 200, 70, P["yellow"]))
    svg += _m(circle(780, 170, 22, P["red"]))

    svg += _m(ellipse(1150, 300, 110, 80, P["yellow"]))

    svg += _m(circle(1350, 180, 75, P["pink"]))
    svg += _m(circle(1380, 230, 30, P["blue"]))
    svg += _m(circle(1310, 150, 20, P["red"]))


    svg += _m(circle(450, 130, 35, P["blue"]))
    svg += _m(circle(1020, 250, 45, P["red"]))
    svg += _m(circle(100, 370, 30, P["green"]))
    svg += _m(circle(1430, 380, 25, P["green"]))

    svg += _m(rrect(580, 100, 80, 50, P["red"], rx=25))
    svg += _m(rrect(1250, 350, 100, 60, P["blue"], rx=30))












    svg += "</svg>\n"
    return svg


def main():
    parser = argparse.ArgumentParser(description="Generate crosslink banner SVG")
    parser.add_argument("-o", "--output", help="Output file (default: stdout)")
    parser.add_argument("--png", action="store_true", help="Convert to PNG (requires cairosvg)")
    args = parser.parse_args()

    svg_content = generate()

    if args.png:
        try:
            import cairosvg
        except ImportError:
            print("Error: pip install cairosvg for PNG output", file=sys.stderr)
            sys.exit(1)
        png_data = cairosvg.svg2png(bytestring=svg_content.encode(), output_width=WIDTH * 2)
        if args.output:
            with open(args.output, "wb") as f:
                f.write(png_data)
        else:
            sys.stdout.buffer.write(png_data)
    else:
        if args.output:
            with open(args.output, "w") as f:
                f.write(svg_content)
            print(f"Written: {args.output}", file=sys.stderr)
        else:
            print(svg_content)


if __name__ == "__main__":
    main()
