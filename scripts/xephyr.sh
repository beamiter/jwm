Xephyr :2 -screen 1280x800 -ac -noreset

cd /home/yj/projects/jwm
DISPLAY=:2 \
JWM_BACKEND=x11rb \
JWM_SLIME_SOCKET=/tmp/jwm-slime-test.sock \
RUST_LOG=debug \
target/debug/jwm

cd /home/yj/projects/jwm
python3 tools/slime_pose_demo.py \
  --socket /tmp/jwm-slime-test.sock \
  --refract-px 14

python3 tools/slime_pose_demo.py \
  --socket "$XDG_RUNTIME_DIR/jwm-slime.sock" \
  --refract-px 12

python3 tools/slime_pose_demo.py \
  --socket /tmp/jwm-slime-test.sock \
  --refract-px 20 \
  --scale 1.25

python3 tools/slime_pose_demo.py \
  --socket "$XDG_RUNTIME_DIR/jwm-slime.sock" \
  --refract-px 20 \
  --scale 1.25

python3 tools/slime_pose_demo.py \
  --socket /tmp/jwm-slime-test.sock \
  --window 0x40000c \
  --refract-px 14

python3 tools/slime_pose_demo.py \
  --socket "$XDG_RUNTIME_DIR/jwm-slime.sock" \
  --window 0x40000c \
  --refract-px 14

