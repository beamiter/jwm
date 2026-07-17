# WaterLily smoke test (run each block in a separate terminal from the repo root).

Xephyr :2 -screen 1280x800 -ac -noreset

# JWM consumer. Change x11rb to xcb to smoke-test the other supported backend.
DISPLAY=:2 \
JWM_BACKEND=x11rb \
JWM_WATERLILY_ENABLED=1 \
JWM_WATERLILY_SOCKET=/tmp/jwm-waterlily-test.sock \
JWM_WATERLILY_FRAME_FILE=/tmp/jwm-waterlily-test.frame \
RUST_LOG=debug \
target/debug/jwm

# Julia producer. The simulation is scaled by JWM to the complete Xephyr output.
julia --project=waterlily --threads=auto waterlily/runner.jl \
  --case hover \
  --device auto \
  --fps 30 \
  --sim-size 320x200 \
  --socket /tmp/jwm-waterlily-test.sock \
  --frame-file /tmp/jwm-waterlily-test.frame
