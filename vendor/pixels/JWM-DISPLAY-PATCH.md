# Owned native display for GLES presentation

This is the crates.io `pixels` 0.17.2 package, under its original MIT license
(see `LICENSE`). Upstream: <https://github.com/parasyte/pixels>, commit
`47c09b923d9a646fe6d71515edcc497156f4f356` from the published package metadata.
The original `.crate` SHA-256 is
`27af78f16789b973b8b340fe3f68cc2ec9f5e6f4aea64e4d794d5833ac9ca52e`.

wgpu 29 requires a native display handle when creating a GLES instance intended
for window presentation. The upstream builder creates an instance without a
display. On Mesa EGL this selects an explicitly surfaceless platform; a later
X11/Wayland surface cannot make that instance presentable.

The only source change is the additive `PixelsBuilder::display_handle` option
in `src/builder.rs`. It retains an owned, thread-safe handle and puts it in the
existing instance descriptor before initialization. The default is `None`, so
existing callers and borrowed-window APIs remain compatible. JWM's two pixels
frontends pass a clone of the same `Arc<Window>` used by `SurfaceTexture`.
There are no raw-pointer lifetime casts or changes to pixels' render pipeline.
The normalized package manifest also has an empty workspace table so the vendor
package stays independent of JWM's core workspace.

Actual desktop checks and failed pre-patch attempts are retained beneath
`artifacts/header-alignment`; the display is verified with Mesa software GL and
a private Xvfb, without changing the logged-in desktop or rendering framework.
