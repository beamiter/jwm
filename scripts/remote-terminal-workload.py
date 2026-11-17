#!/usr/bin/env python3
"""Bounded deterministic terminal repaint fixture; not a latency measurement."""
import argparse
import math
import sys
import time


def frame(index, rows, columns):
    pattern = 'Il1| 0O 中文标点：，。 αβγ RGB #00ffff fn render() { 123; } '
    # Width is a declared glyph budget, not terminal display-cell width. The
    # terminal may wrap: record its actual size separately with stty size.
    line = (pattern * (columns // len(pattern) + 1))[:columns]
    return '\x1b[H' + '\n'.join(f'{(index + row):06d} {line}' for row in range(rows)) + '\x1b[J'


def run(seconds, fps, rows, columns, output=sys.stdout, clock=time.monotonic, sleep=time.sleep):
    start = clock()
    period = 1 / fps
    try:
        output.write('\x1b[?1049h\x1b[?25l')
        for index in range(math.ceil(seconds * fps)):
            if clock() - start >= seconds:
                break
            output.write(frame(index, rows, columns))
            output.flush()
            sleep(max(0, start + (index + 1) * period - clock()))
    finally:
        output.write('\x1b[0m\x1b[?25h\x1b[?1049l')
        output.flush()


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--seconds', type=int, choices=range(1, 301), default=30, metavar='1..300')
    p.add_argument('--fps', type=int, choices=range(1, 61), default=30, metavar='1..60')
    p.add_argument('--rows', type=int, choices=range(1, 101), default=24, metavar='1..100')
    p.add_argument('--columns', type=int, choices=range(8, 241), default=80, metavar='8..240')
    args = p.parse_args()
    if not sys.stdout.isatty():
        p.error('stdout must be a terminal; do not embed escape sequences in evidence logs')
    try:
        run(args.seconds, args.fps, args.rows, args.columns)
    except KeyboardInterrupt:
        return 130
    return 0


if __name__ == '__main__':
    sys.exit(main())
