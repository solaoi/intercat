"""Tie helper processes to the daemon's private input pipe."""
import os
import sys
from threading import Thread


def watch_parent():
    def wait_for_close():
        while os.read(sys.stdin.fileno(), 1):
            pass
        os._exit(2)
    Thread(target=wait_for_close, daemon=True).start()
