#!/usr/bin/env python3
"""Pure arithmetic bound, not a production scheduler or timing benchmark.

Constants mirror the current default tile size, PNG pixel/dimension ceilings,
repair cooldown and nonrenewable lease. This does not read a display or clock.
"""
import argparse
import json

TILE = 16
MAX_ATLAS_PIXELS = 65536
MAX_ATLAS_DIMENSION = 16384
IDLE_MS = 2000
LEASE_MS = 30000


def budget(width, height):
    if type(width) is not int or type(height) is not int:
        raise ValueError("integer dimensions required")
    if not 1 <= width <= MAX_ATLAS_DIMENSION or not 1 <= height <= MAX_ATLAS_DIMENSION:
        raise ValueError("dimensions outside model bounds")
    tiles = ((width + TILE - 1) // TILE) * ((height + TILE - 1) // TILE)
    batch = min(MAX_ATLAS_PIXELS // (TILE * TILE), MAX_ATLAS_DIMENSION // TILE)
    batches = (tiles + batch - 1) // batch
    earliest = batches * IDLE_MS
    return dict(width=width, height=height, tiles=tiles,
                padded_pixels=tiles * TILE * TILE, tiles_per_batch=batch,
                batches=batches, ideal_lower_bound_ms=earliest,
                impossible_before_lease_expiry=earliest >= LEASE_MS)


def self_test():
    assert budget(200, 150)["tiles"] == 130
    assert budget(800, 600)["batches"] == 8
    assert budget(1280, 720)["ideal_lower_bound_ms"] == LEASE_MS
    assert budget(1280, 720)["impossible_before_lease_expiry"]
    hd = budget(1920, 1080)
    assert hd["tiles"] == 8160 and hd["padded_pixels"] == 2088960
    assert hd["batches"] == 32 and hd["ideal_lower_bound_ms"] == 64000
    assert budget(16, 16)["tiles"] == 1
    assert budget(17, 17)["tiles"] == 4
    # Strict deadline: hypothetical completions at 2,4,...,28s only.
    assert (LEASE_MS - 1) // IDLE_MS == 14
    for dims in [(True, 16), (0, 16), (16385, 16)]:
        try:
            budget(*dims)
        except ValueError:
            continue
        raise AssertionError("invalid dimensions accepted")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    self_test()
    if args.self_test:
        print("pointer lossless arithmetic checks passed")
    else:
        print(json.dumps({"scope": "ideal arithmetic lower bounds; no phase/CPU/network costs; not observed timings",
                          "lease_ms": LEASE_MS, "cooldown_ms": IDLE_MS,
                          "rows": [budget(w, h) for w, h in [(200, 150), (800, 600), (1280, 720), (1920, 1080)]]}, indent=2))
