# P5A: 10-bit GLX Output Context for HDR - Testing Guide

## Overview
This guide covers testing the P5A HDR implementation (Phases 1-3).

**Status**: Phase 1-3 implementation complete
- Phase 1: RandR HDR detection ✅
- Phase 2: 10-bit GLX FBConfig selection ✅
- Phase 3: 10-bit TFP FBConfig support ✅

---

## Pre-Flight Checklist

### Compilation
```bash
cargo build --release
```
Expected: Builds successfully with only warnings (no errors)

### Configuration
Create/update `~/.config/jwm/config.toml`:
```toml
[behavior]
hdr_enabled = true              # Enable 10-bit output
hdr_peak_nits = 400             # HDR400 baseline
tone_mapping_method = "aces"    # ACES filmic curve
```

---

## Testing Scenarios

### 1. Basic Startup (All Systems)

**Test**: Start compositor with HDR enabled
```bash
# Terminal 1: Start compositor
RUST_LOG=info ~/.cargo/target/release/jwm

# Look for these logs:
# - "HDR output requested, selected FBConfig visual depth=30" (or similar)
# - "TFP FBConfigs: rgba=... rgb=... hdr_10bit=true" (if 10-bit available)
```

**Expected**: 
- ✅ Compositor starts cleanly
- ✅ No crashes on 8-bit-only systems (graceful fallback)
- ✅ HDR logs appear in output

**On 8-bit-only systems**:
- ✅ Logs should show "visual depth=24" (fallback)
- ✅ No errors, continues normally
- ✅ Functionality unchanged

---

### 2. HDR Capability Detection (X11 Only)

**Test**: Query RandR for HDR support
```bash
# Check if your monitor reports HDR support
xrandr --verbose | grep -A 5 "HDR\|max_bpc\|output_bpc"

# Or use X11 tools
xprop -root | grep "max_bpc"
```

**Expected**:
- On HDR displays: should report `max_bpc >= 10`
- On SDR displays: may report `max_bpc = 8` or no property
- Compositor should still work either way (config flag overrides detection)

---

### 3. GLX Context Verification

**Test**: Check actual GLX visual depth
```bash
glxinfo 2>/dev/null | grep -i "visual\|color bits\|depth"

# Look for patterns like:
# - visual 0x... depth=30 (ideal, 10-bit)
# - visual 0x... depth=24 (fallback, 8-bit)
```

**Expected**:
- With `hdr_enabled=true` on HDR system: depth should be 30 or higher
- With `hdr_enabled=false`: depth should be 24 (standard)
- With `hdr_enabled=true` on SDR system: depth may be 24 (graceful fallback)

---

### 4. Performance Regression Test

**Test**: Measure frame time with/without HDR
```bash
# Terminal 1: Run with HDR enabled
hdr_enabled=true jwm-tool ipc '{"query": "get_metrics"}' | \
  jq '.data | {fps, avg_frame_time_ms, gpu_load_percent}'

# Terminal 1: Run with HDR disabled
sed -i 's/hdr_enabled = true/hdr_enabled = false/' ~/.config/jwm/config.toml
jwm-tool ipc '{"query": "get_metrics"}' | \
  jq '.data | {fps, avg_frame_time_ms, gpu_load_percent}'
```

**Expected**:
- Frame time should be unchanged (< 1% difference)
- GPU load should be unchanged
- FPS stable in both cases

---

### 5. Visual Quality Test (HDR Monitor Only)

**Requires**: An actual HDR monitor

**Test**: Display HDR content
```bash
# Option A: Use a 10-bit test pattern
ffplay -autoexit -pixel_format yuv420p10le test_10bit.mkv

# Option B: Use video with HDR metadata
ffplay video_with_hdr_metadata.mkv
```

**Expected**:
- Video displays without color banding
- Colors appear smooth and natural
- No visual artifacts compared to playing without compositor

**On SDR monitor** (simulate):
- Image should display tone-mapped to SDR range
- No color shift or artifacts
- Indistinguishable from non-HDR rendering

---

### 6. Log Analysis

**Test**: Review detailed HDR initialization logs
```bash
# Start compositor with debug logging
RUST_LOG=debug ~/.cargo/target/release/jwm 2>&1 | grep -i "hdr\|10.?bit\|tfp\|visual depth"
```

**Expected output patterns**:
```
compositor: HDR output requested, selected FBConfig visual depth=30
compositor: TFP FBConfigs: rgba=1 rgb=1 per_visual=N hdr_10bit=true
```

**On 8-bit system**:
```
compositor: HDR output requested, selected FBConfig visual depth=24 (fallback, 10-bit unavailable)
compositor: TFP FBConfigs: rgba=1 rgb=1 per_visual=N hdr_10bit=false
```

---

## Diagnostic Commands

### Check HDR Config
```bash
grep -A 3 "hdr_enabled\|hdr_peak_nits\|tone_mapping" ~/.config/jwm/config.toml
```

### Query Compositor Metrics
```bash
jwm-tool ipc '{"query": "get_metrics"}' | jq '.data | {
  fps,
  avg_frame_time_ms,
  gpu_load_percent,
  blur_quality,
  vrr_active,
  blur_cache_hit_rate
}'
```

### Monitor Display Capabilities
```bash
# X11 + RandR
xrandr --props | grep -B 5 "max_bpc\|HDR"

# Or check with drm
cat /sys/class/drm/card*/device/driver

# EDID information
parse-edid < /sys/class/drm/card0/card0-*/edid
```

### Check OpenGL Support
```bash
glxinfo | grep -i "glx version\|renderer\|opengl version"
```

---

## Rollback / Disable

If any issues occur, disable HDR with:
```bash
# Option 1: Edit config
sed -i 's/hdr_enabled = true/hdr_enabled = false/' ~/.config/jwm/config.toml

# Option 2: Temporary override
RUST_LOG=info jwm
# Then Ctrl+C and fix config
```

---

## Known Limitations (Expected Behavior)

1. **10-bit FBConfig May Not Be Available**
   - Older GPU drivers may not support 10-bit visual
   - Fallback to 8-bit is transparent and automatic
   - No performance penalty

2. **HDR Output Requires Compatible Display**
   - 10-bit output only works with HDR-capable monitors
   - SDR monitors display correctly but without 10-bit precision
   - No way to detect if tone-mapping is actually used

3. **X11-Only (For Now)**
   - Wayland HDR support would require separate implementation
   - X11 backend fully supports P5A

---

## Success Criteria

✅ Compositor compiles without errors
✅ Starts with `hdr_enabled=true` on all systems
✅ Logs show "HDR output requested" message
✅ No performance regression (frame time unchanged)
✅ No crashes on 8-bit or HDR systems
✅ Graceful fallback when 10-bit FBConfig unavailable
✅ (Optional) HDR content displays correctly on HDR monitors

---

## Next Steps

### For P5B: Per-Monitor Blur Quality
- Detect primary vs secondary display
- Apply reduced blur on secondary monitors
- Expected: 5-10% frame time improvement on multi-display setups

### For P5C: Dirty Rectangle Optimization
- Fine-grained region tracking
- Only re-render changed pixels
- Expected: 10-20% improvement on partial window updates

### For P6 (Future): 10-bit HDR Output Context
- Requires 30-bit X Visual setup
- Monitor-specific HDR capability query
- Per-window HDR hints via _NET_WM_WINDOW_TYPE

---

## Troubleshooting

### Compositor won't start
- Check: `cargo build --release` succeeds
- Check: Config file is valid TOML
- Try: `hdr_enabled = false` temporarily

### HDR logs not appearing
- Check: `RUST_LOG=info` is set
- Check: Compositor was restarted after config change
- Check: Monitor/GPU actually supports HDR

### Visual corruption
- Check: No unrelated changes to blur pipeline
- Check: GPU drivers up to date
- Try: `hdr_enabled = false` to isolate issue

### Performance degradation
- Check: Frame time metrics via `jwm-tool ipc`
- Check: GPU load is not >95%
- Check: Blur quality not forced to `Full` on all windows

---

## Questions / Issues

See /home/mm/.claude/plans/structured-sleeping-cocke.md for architecture details.
