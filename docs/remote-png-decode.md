# PNG repair decode resource reuse

This change is independent of the experimental lossless scheduler. It does not
change the wire format, negotiated permissions, frame selection or presentation
proofs, and does not enable RegionDeadline or RegionPriority.

The tile decoder now reuses its private RGB atlas buffer for PNG repair records.
After the first allocation, equal or smaller PNG repairs reuse the allocation;
a larger PNG grows it fallibly. A valid PNG following a larger JPEG discards
scratch exceeding the 196,608-byte PNG bound, rather than retaining a full-frame
JPEG allocation indefinitely. PNG decoder internals may still allocate. This
is not an allocation-free decoder or a total-process memory bound.

Envelope CRC validation uses a fixed, compile-time 1 KiB IEEE CRC table instead
of eight polynomial rounds per input byte. Strict chunk/order/format/size checks,
CRC coverage, zlib allocation limits and exact decoded output checks remain.
Both encoder self-validation and receiver envelope validation use this path.
CRC is error detection, not authentication; the authenticated transport remains
unchanged.

On any decode error, the reusable output has zero logical length. The tile
canvas, snapshot and sequence remain unchanged because they are updated only
after successful complete decoding. The existing decoder may tolerate extra
internal zlib data; that prior compatibility limit is unchanged.

## Verification

- Known IEEE CRC vector and reference equivalence for every byte and bounded
  mixed input up to the 1 MiB compressed limit.
- Every single-bit corruption of a valid small PNG is rejected.
- Repeated maximum/small/growing PNG repairs preserve allocation identity and
  exactly overwrite every output pixel.
- Invalid envelopes and valid-envelope/invalid-zlib input clear private output;
  the next valid image recovers with exact pixels.
- Oversized preceding JPEG scratch is released on the next valid PNG decode.
- Existing strict PNG, tile codec and lossless integration suites remain relevant.

Run unit tests with the repository test wrapper:

```
scripts/test.sh --no-default-features --features remote-wayland,backend-wayland-udev --lib remote::png_atlas::tests::
```

The ignored `benchmark_png_crc_validation` test compares the old bitwise CRC
and new lookup loop over identical input. For meaningful timing, use the
optimized `profiling` profile and run without other builds. It measures only
CRC, not PNG decompression, capture, network, GL or input-to-visible latency.
No end-to-end performance improvement is claimed from that microbenchmark.
