"""Opt-in syscall capture for the private media acceptance fixture only."""
import os
from pathlib import Path
import shlex
import shutil
import sys


def launcher_prefix(evidence):
    tracer = shutil.which("strace")
    if not tracer:
        raise RuntimeError("--trace-mpv requires strace; refusing untraced media test")
    # -D retains the original tracee PID and parent; -f includes mpv's IPC
    # thread. Full fd0 dumps distinguish JSON from unexpected binary bytes.
    # Keep other buffers bounded and all evidence inside this private fixture.
    arguments = [tracer, "-D", "-f", "-ttt", "-yy", "-xx", "-s", "256",
                 "-e", "trace=read,readv,write,writev,recvfrom,recvmsg,sendto,sendmsg,close,close_range,dup,dup2,dup3,fcntl,openat,socket,socketpair,connect,pipe,pipe2,execve,exit,exit_group",
                 "-e", "read=0", "-e", "write=0", "-o"]
    return (" ".join(map(shlex.quote, arguments)) + " "
            + shlex.quote(str(evidence / "mpv-syscalls-")) + '"$$".log '
            + shlex.quote(sys.executable) + " " + shlex.quote(str(Path(__file__).resolve()))
            + ' --guard "$$" "$PPID" ')


def guarded_exec(arguments):
    expected_pid, expected_parent = map(int, arguments[:2])
    tracer = next(int(line.split()[1]) for line in Path("/proc/self/status").read_text().splitlines()
                  if line.startswith("TracerPid:"))
    # strace -D can still run its command after a failed ptrace attach. Never
    # mistake that for a captured run; preserve mpv's parent/PID contract too.
    if not tracer or os.getpid() != expected_pid or os.getppid() != expected_parent:
        raise RuntimeError("mpv syscall capture unavailable or changed process identity; refusing untraced execution")
    os.execv(arguments[2], arguments[2:])


if __name__ == "__main__":
    if len(sys.argv) < 5 or sys.argv[1] != "--guard":
        raise SystemExit("private fixture usage: --guard PID PPID PROGRAM [ARG ...]")
    guarded_exec(sys.argv[2:])
