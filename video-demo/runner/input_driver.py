from __future__ import annotations

import math
import shutil
import subprocess
import time


class XdotoolInput:
    def __init__(self) -> None:
        self.binary = shutil.which("xdotool")
        if not self.binary:
            raise RuntimeError("xdotool is required for pointer-driven compositor scenes")

    def _run(self, *args: object) -> None:
        subprocess.run([self.binary, *(str(arg) for arg in args)], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    def move_path(self, points: list[tuple[int, int]], duration: float = 1.5) -> None:
        if not points: return
        delay = duration / max(1, len(points))
        for x, y in points:
            # Do not use `--sync` for every sample: under wobbly/animated
            # movement XTest can wait for compositor settling at each point,
            # stretching a 2.5s gesture into tens of seconds.
            self._run("mousemove", x, y)
            time.sleep(delay)

    def smooth(self, start: tuple[int, int], end: tuple[int, int], steps: int = 45, duration: float = 1.5) -> list[tuple[int, int]]:
        points = []
        for index in range(steps + 1):
            t = index / steps
            eased = 0.5 - 0.5 * math.cos(math.pi * t)
            points.append((round(start[0] + (end[0] - start[0]) * eased), round(start[1] + (end[1] - start[1]) * eased)))
        self.move_path(points, duration)
        return points

    def drag(self, start: tuple[int, int], end: tuple[int, int], duration: float = 2.0, modifier: str | None = "Alt_L") -> None:
        self._run("mousemove", *start)
        if modifier: self._run("keydown", modifier)
        self._run("mousedown", 1)
        try: self.smooth(start, end, duration=duration)
        finally:
            self._run("mouseup", 1)
            if modifier: self._run("keyup", modifier)
