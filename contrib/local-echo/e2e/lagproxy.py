"""Relay stdin/stdout to a wezterm mux server's unix socket, delaying each
direction by half of the given round-trip time (ms). Order is preserved.

usage: lagproxy.py <rtt-ms> <socket-path>

LAG_JITTER_MS adds up to that much extra random delay per chunk; order is
still preserved, so later chunks queue behind slow ones and arrive in bursts
like on a real long-distance link.
"""
import os, queue, random, socket, sys, threading, time

one_way = float(sys.argv[1]) / 2000.0
START = time.monotonic()
TRACE = os.environ.get('LAG_TRACE')
JITTER = float(os.environ.get('LAG_JITTER_MS', '0')) / 1000.0

sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
sock.connect(sys.argv[2])


def relay(read, write, close, name):
    q = queue.Queue()

    def reader():
        while True:
            try:
                data = read()
            except OSError:
                data = b""
            t = time.monotonic()
            if TRACE:
                with open(TRACE, 'a') as f:
                    f.write('%8.3f %-4s %5d %s\n' % (t - START, name, len(data), data[:24].hex()))
            q.put((t + one_way + random.uniform(0, JITTER), data))
            if not data:
                return

    threading.Thread(target=reader, daemon=True).start()
    while True:
        due, data = q.get()
        wait = due - time.monotonic()
        if wait > 0:
            time.sleep(wait)
        if not data:
            close()
            return
        write(data)


def write_stdout(data):
    view = memoryview(data)
    while view:
        view = view[os.write(1, view):]


up = threading.Thread(
    target=relay,
    args=(lambda: os.read(0, 65536), sock.sendall, lambda: sock.shutdown(socket.SHUT_WR), 'up'),
    daemon=True)
down = threading.Thread(
    target=relay,
    args=(lambda: sock.recv(65536), write_stdout, lambda: os.close(1), 'down'),
    daemon=True)
up.start()
down.start()
down.join()
