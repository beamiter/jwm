# Local speech detector fixture

`rnnoise-speech.s16le` is a 1.8-second excerpt of the upstream nnnoiseless
`test_data/mono.wav` regression recording. It is 24,000 Hz, mono, signed
16-bit little-endian PCM, with no header. This is public upstream test audio,
not a user's microphone recording.

Source pinned to nnnoiseless 0.5.2:

- [Original audio](https://github.com/jneem/nnnoiseless/blob/924a2dd143ccad7bce9e5bda061b60ca32911a67/test_data/mono.wav)
- [Upstream BSD-3-Clause license](https://github.com/jneem/nnnoiseless/blob/924a2dd143ccad7bce9e5bda061b60ca32911a67/COPYING)

The excerpt preserves the upstream recording's background noise. Tests also
attenuate it and add deterministic generated noise; these are narrow acoustic
regressions, not evidence of real-device or speaker-independent accuracy.

Reproduction:

```bash
ffmpeg -v error -nostdin -i mono.wav -ss 0.3 -t 1.8 \
  -ar 24000 -ac 1 -f s16le rnnoise-speech.s16le
```

SHA-256:

```text
Original mono.wav:
7bfc09f923b8c766354fe2e9fb6c07d9781f06c68d13ba14046b61f54c6257fc
rnnoise-speech.s16le:
c834f2a64448bf0128793839e05397a00a3b2b14ec9444e9aeccee17f11ecc56
```

## Upstream license

Copyright (c) 2020, Joe Neeman
Copyright (c) 2017, Mozilla
Copyright (c) 2007-2017, Jean-Marc Valin
Copyright (c) 2005-2017, Xiph.Org Foundation
Copyright (c) 2003-2004, Mark Borgerding

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions
are met:

- Redistributions of source code must retain the above copyright
  notice, this list of conditions and the following disclaimer.
- Redistributions in binary form must reproduce the above copyright
  notice, this list of conditions and the following disclaimer in the
  documentation and/or other materials provided with the distribution.
- Neither the name of the Xiph.Org Foundation nor the names of its
  contributors may be used to endorse or promote products derived from
  this software without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
``AS IS'' AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
A PARTICULAR PURPOSE ARE DISCLAIMED.  IN NO EVENT SHALL THE FOUNDATION
OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT
LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
(INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
