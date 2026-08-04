#!/bin/bash
# Performance measurement script for x11rb_bar optimization

set -e

DURATION=${1:-30}  # Default 30 seconds
OUTPUT_PREFIX=${2:-perf_results}

echo "=== x11rb_bar Performance Measurement ==="
echo "Duration: ${DURATION}s"
echo "Output prefix: ${OUTPUT_PREFIX}"
echo ""

# Make sure x11rb_bar is built
echo "Building x11rb_bar..."
cargo build --release 2>&1 | grep -E "Compiling|Finished|error" || true

# Kill any existing x11rb_bar instances
pkill -f "target/release/x11rb_bar" || true
sleep 1

# Start x11rb_bar in background
echo "Starting x11rb_bar..."
DISPLAY=:0 ./target/release/x11rb_bar &
PID=$!
sleep 2

# Run performance measurement
echo "Measuring performance for ${DURATION} seconds..."
echo ""

# CPU cycles and instructions (requires perf)
if command -v perf &> /dev/null; then
    echo "CPU metrics (cycles, instructions):"
    perf stat -e cycles,instructions,cache-references,cache-misses -p $PID sleep $DURATION 2>&1 | grep -E "cycles|instructions|cache"
    echo ""
fi

# Memory usage
echo "Memory usage:"
ps aux | grep "x11rb_bar" | grep -v grep | awk '{printf "VSZ: %d KB, RSS: %d KB\n", $5, $6}'
echo ""

# System load
echo "Average system load:"
uptime | awk -F'average: ' '{print $2}'
echo ""

# Check for any crashes/errors in logs
if [ -f ~/.xbar_core/x11rb_bar.log ]; then
    echo "Recent log entries:"
    tail -20 ~/.xbar_core/x11rb_bar.log | grep -E "ERROR|WARN" || echo "(No errors)"
else
    echo "Log file not found"
fi

# Cleanup
echo ""
echo "Stopping x11rb_bar..."
kill $PID 2>/dev/null || true
sleep 1

echo "=== Measurement Complete ==="
