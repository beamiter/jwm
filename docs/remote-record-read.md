# Combined authenticated record reads

The authenticated record wire format is unchanged. After reading and validating
the 13-byte header, the receiver reads ciphertext and its fixed 16-byte tag into
one reusable buffer. It copies the tag to a fixed-size value, truncates the buffer
to ciphertext length, authenticates/decrypts, then removes padding.

## Bounds and failure behavior

The existing negotiated ciphertext cap is checked before resizing or reading the
body. Temporary logical length is at most the global 32 MiB cap plus 16 bytes.
The tag does not count against the negotiated ciphertext cap. Sequence, message
kind, authentication and padding checks are unchanged. Every error clears the
visible payload; oversized retained buffers are released as before. Callers must
close the session after a protocol error, not retry a partially consumed record.
No prefetch reads beyond the current record, buffering thread or wire change is
introduced. Socket and phase deadlines retain their existing behavior.

## Verification and limits

A counting reader with all bytes available observes two `Read::read` calls per
record (header, combined body), instead of three (header, ciphertext, tag).
This removes one mandatory read boundary; TCP fragmentation may still require
many reads. It is not an end-to-end latency, throughput or physical-network
benchmark.

Tests cover padding boundaries, consecutive records, exact record consumption,
fragmented and interrupted reads, allocation reuse, every truncation point of a
small record, all tag-byte corruption positions, and ciphertext-cap enforcement
before body allocation/read. Existing authenticated TCP and client/sender tests
cover the unchanged transport integration.

A caller buffer that fits ciphertext exactly may grow once to accommodate the
additional 16 bytes; allocator capacity can exceed that logical length. Later
same-size records reuse the enlarged buffer. This change does not promise zero
allocations at a capacity boundary or change the existing retention policy.
