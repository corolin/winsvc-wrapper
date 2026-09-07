# Long-running service fixture for runtime tests.
import signal
import sys
import time

for i, a in enumerate(sys.argv[1:]):
    print(f"ARG{i}={a}", flush=True)
print("READY", flush=True)


def on_sig(signum, frame):
    print("CLEAN-SHUTDOWN", flush=True)
    sys.exit(0)


signal.signal(signal.SIGINT, on_sig)
n = 0
while True:
    time.sleep(0.3)
    print(f"TICK {n}", flush=True)
    n += 1
