# Initialized image decode destinations

Reusing allocation capacity must not reuse the previous image's pixels. The
full-frame RGB decode pool now initializes the complete logical destination
before each attempt. Equal-size and growing destinations follow the same rule.
The tile JPEG decoder retains its existing initialization; PNG repair decoding
also preserves the fresh-buffer initialization behavior while reusing capacity.

This is defensive output isolation, not a performance optimization or a new
JPEG dialect. Allocation reuse, frame limits, codec negotiation, source scope,
canvas publication and presentation proofs remain unchanged. Decoder errors
still cannot publish a partial frame. RegionDeadline and RegionPriority remain
test-only.

Generic regressions cover same-size and growing pool reuse, a successful writer
that fills only a prefix, actual allocation identity, and error recovery. A
successful partial writer can leave only initialized zeros in the untouched
suffix, never pixels from the prior buffer owner. Existing frame, tile, PNG and
real lossless-family tests remain part of verification.
