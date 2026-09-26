"""Local CLI orchestration rehearsal with a deterministic Chat server; no API key."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        messages = body["messages"]
        first = next(message["content"] for message in messages if message["role"] == "user")
        count = sum(message["role"] == "tool" for message in messages)
        message = {"role": "assistant", "content": "Review: counter.py loses concurrent updates; add synchronization and a concurrent test."}
        finish = "stop"
        if not first.startswith("Act as the parent") and count < 4:
            name = "ask_parent" if count == 0 else "workspace_read_file"
            args = {"prompt": "Which priority?"} if count == 0 else {"path": ["README.md", "counter.py", "test_counter.py"][count - 1]}
            message = {"role": "assistant", "content": None, "tool_calls": [
                {"id": f"call_{count}", "type": "function", "function": {"name": name, "arguments": json.dumps(args)}}]}
            finish = "tool_calls"
        data = json.dumps({"choices": [{"index": 0, "finish_reason": finish, "message": message}],
                           "usage": {"prompt_tokens": 10, "completion_tokens": 5}}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


class SwarmDemoTest(unittest.TestCase):
    def test_three_reviewers_parent_resume_tools_and_synthesis(self):
        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        evidence = None
        try:
            with tempfile.TemporaryDirectory() as directory:
                config = Path(directory) / "provider.toml"
                config.write_text(f'[provider]\nbase_url = "http://127.0.0.1:{server.server_port}"\nmodel = "mock"\napi_key_env = "SWARM_DEMO_TEST_KEY"\n')
                result = subprocess.run(["python3", "examples/swarm-demo.py", "--config", str(config)],
                                        capture_output=True, text=True, timeout=30,
                                        env={**os.environ, "SWARM_DEMO_TEST_KEY": "dummy", "NO_PROXY": "127.0.0.1", "no_proxy": "127.0.0.1"})
                if result.stdout.startswith("Evidence directory: "):
                    evidence = Path(result.stdout.splitlines()[0].removeprefix("Evidence directory: "))
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                summary = json.loads((evidence / "summary.json").read_text())
                self.assertEqual(len(summary["runs"]), 4)
                self.assertEqual(sum(row["usage"]["model_requests"] for row in summary["runs"].values()), 16)
                questions = json.loads((evidence / "parent-questions.json").read_text())
                self.assertEqual(len(questions), 3)
                tools = json.loads((evidence / "tool-evidence.json").read_text())
                self.assertEqual(len(tools), 3)
        finally:
            server.shutdown()
            server.server_close()
            thread.join()
            if evidence:
                shutil.rmtree(evidence)


if __name__ == "__main__":
    unittest.main()
