#!/usr/bin/env python3
"""用真实 curl 验证 kine 下载停滞时有界重试，且不覆盖已有文件。"""

import http.server
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import tempfile
import threading
import time
import unittest


class FetchKineTest(unittest.TestCase):
    def test_stalled_download_is_bounded_retried_and_preserves_destination(self) -> None:
        requests: list[float] = []
        stopped = threading.Event()

        class StalledBody(http.server.BaseHTTPRequestHandler):
            def do_GET(self) -> None:
                requests.append(time.monotonic())
                self.send_response(200)
                self.send_header("Content-Length", "1024")
                self.end_headers()
                self.wfile.write(b"partial")
                self.wfile.flush()
                stopped.wait(15)

            def log_message(self, _format: str, *args: object) -> None:
                pass

        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), StalledBody)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        curl = shutil.which("curl")
        self.assertIsNotNone(curl)
        with tempfile.TemporaryDirectory(prefix="fetch-kine-test-") as directory:
            root = Path(directory)
            destination = root / "kine"
            destination.write_bytes(b"existing-verified-install")
            wrapper = root / "curl"
            # 仅替换外部地址并缩短等待时间；实际 HTTP、超时、重试和退出码仍由 curl 执行。
            wrapper.write_text(
                "#!/usr/bin/env python3\n"
                "import json, os, sys\n"
                "args = sys.argv[1:]\n"
                "open(os.environ['FETCH_TEST_ARGS'], 'w').write(json.dumps(args))\n"
                "for flag, scale in [('--connect-timeout', 0.01), ('--max-time', 0.001)]:\n"
                "    if flag in args:\n"
                "        i = args.index(flag) + 1\n"
                "        args[i] = str(float(args[i]) * scale)\n"
                "args[-1] = os.environ['FETCH_TEST_URL']\n"
                "os.execv(os.environ['FETCH_TEST_CURL'], [os.environ['FETCH_TEST_CURL'], '-q', *args])\n"
            )
            wrapper.chmod(0o755)
            arguments = root / "arguments.json"
            script = Path(__file__).resolve().with_name("fetch-kine.sh")
            started = time.monotonic()
            process = subprocess.Popen(
                ["bash", str(script), str(destination)],
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                start_new_session=True,
                env={
                    **os.environ,
                    "PATH": f"{root}:{os.environ['PATH']}",
                    "FETCH_TEST_ARGS": str(arguments),
                    "FETCH_TEST_URL": f"http://127.0.0.1:{server.server_port}/kine",
                    "FETCH_TEST_CURL": str(curl),
                    "NO_PROXY": "127.0.0.1",
                },
            )
            timed_out = False
            try:
                _, stderr = process.communicate(timeout=12)
            except subprocess.TimeoutExpired:
                timed_out = True
                os.killpg(process.pid, signal.SIGTERM)
                _, stderr = process.communicate(timeout=2)
            finally:
                stopped.set()
                server.shutdown()
                server.server_close()
                thread.join(timeout=2)

            self.assertFalse(timed_out, "the stalled fetch outlived its download/retry budget")
            self.assertEqual(process.returncode, 28, stderr.decode())
            self.assertLess(time.monotonic() - started, 12)
            self.assertEqual(len(requests), 4, "one attempt plus the existing three retries")
            self.assertEqual(destination.read_bytes(), b"existing-verified-install")
            args = json.loads(arguments.read_text())
            for flag, value in [("--connect-timeout", "10"), ("--max-time", "300")]:
                self.assertIn(flag, args)
                self.assertEqual(args[args.index(flag) + 1], value)


if __name__ == "__main__":
    unittest.main()
