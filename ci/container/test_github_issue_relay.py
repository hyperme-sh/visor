import asyncio
import importlib.util
from pathlib import Path
import stat
import tempfile
import unittest
from unittest.mock import patch


relay_path = Path(__file__).resolve().parents[2] / "container/github-issue-relay.py"
spec = importlib.util.spec_from_file_location("github_issue_relay", relay_path)
relay = importlib.util.module_from_spec(spec)
spec.loader.exec_module(relay)


class GitHubIssueRelayTest(unittest.IsolatedAsyncioTestCase):
    async def test_private_socket_forwards_to_the_fixed_github_endpoint(self):
        async def provider(reader, writer):
            async with asyncio.timeout(2):
                self.assertEqual(await reader.readexactly(5), b"hello")
                writer.write(b"world")
                await writer.drain()
            writer.close()

        open_connection = asyncio.open_connection
        async with await asyncio.start_server(provider, "127.0.0.1", 0) as server:
            port = server.sockets[0].getsockname()[1]

            async def connect(host, destination):
                self.assertEqual((host, destination), ("api.github.com", 443))
                return await open_connection("127.0.0.1", port)

            with tempfile.TemporaryDirectory() as root, patch.object(
                asyncio, "open_connection", connect
            ):
                socket = Path(root) / "github.sock"
                serving = asyncio.create_task(relay.serve(socket))
                try:
                    async with asyncio.timeout(2):
                        while not socket.is_socket():
                            await asyncio.sleep(0.01)
                        self.assertEqual(stat.S_IMODE(socket.stat().st_mode), 0o600)
                        reader, writer = await asyncio.open_unix_connection(socket)
                        try:
                            writer.write(b"hello")
                            await writer.drain()
                            self.assertEqual(await reader.readexactly(5), b"world")
                        finally:
                            writer.close()
                            await writer.wait_closed()
                finally:
                    serving.cancel()
                    with self.assertRaises(asyncio.CancelledError):
                        await serving
                self.assertFalse(socket.exists())
