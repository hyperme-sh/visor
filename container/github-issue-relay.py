#!/usr/bin/env python3
"""Forward a private Unix socket to GitHub's API without terminating TLS."""
import argparse
import asyncio
import os
from pathlib import Path
import signal
import socket


async def copy(reader, writer):
    while data := await reader.read(64 * 1024):
        writer.write(data)
        await writer.drain()
    writer.write_eof()


async def serve(path):
    connections = asyncio.Semaphore(8)
    stop = asyncio.Event()
    for signum in (signal.SIGINT, signal.SIGTERM):
        asyncio.get_running_loop().add_signal_handler(signum, stop.set)

    async def forward(local_reader, local_writer):
        remote_writer = None
        try:
            if connections.locked():
                return
            async with connections, asyncio.timeout(10):
                remote_reader, remote_writer = await asyncio.open_connection("api.github.com", 443)
                try:
                    async with asyncio.TaskGroup() as pipes:
                        pipes.create_task(copy(local_reader, remote_writer))
                        pipes.create_task(copy(remote_reader, local_writer))
                except* OSError:
                    pass
        except (OSError, TimeoutError):
            pass
        finally:
            if remote_writer is not None:
                remote_writer.close()
            local_writer.close()

    with socket.socket(socket.AF_UNIX) as listener:
        # bind refuses an existing path. Only the socket owner can connect.
        mask = os.umask(0o177)
        try:
            listener.bind(str(path))
        finally:
            os.umask(mask)
        inode = path.stat().st_ino
        try:
            async with await asyncio.start_unix_server(forward, sock=listener, backlog=8):
                await stop.wait()
        finally:
            try:
                if path.lstat().st_ino == inode:
                    path.unlink()
            except FileNotFoundError:
                pass


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("socket", type=Path)
    path = parser.parse_args().socket
    if not path.is_absolute():
        parser.error("socket path must be absolute")
    asyncio.run(serve(path))
