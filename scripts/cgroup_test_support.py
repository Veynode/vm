"""Small real stdio MCP client shared by Linux cgroup acceptance scripts."""
import json
from pathlib import Path
import queue
import subprocess
import threading
ROOT = Path(__file__).resolve().parents[1]

class MCP:
    def __init__(self, state, execute=True):
        self.process = subprocess.Popen([str(ROOT / 'target/debug/vm-control'), 'mcp',
            '--state-dir', str(state), '--allow-lifecycle', *(['--allow-execute'] if execute else [])],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, bufsize=1)
        self.sequence = 0
        self.replies = queue.Queue()

        def reader():
            for line in self.process.stdout:
                self.replies.put(json.loads(line))
        threading.Thread(target=reader, daemon=True).start()
        self.request('initialize', {'protocolVersion': '2025-06-18', 'capabilities': {},
                                  'clientInfo': {'name': 'installed-job-validator', 'version': '1'}})
        self.process.stdin.write(json.dumps({'jsonrpc': '2.0', 'method': 'notifications/initialized'}) + '\n')
        self.process.stdin.flush()

    def request(self, method, params):
        self.sequence += 1
        self.process.stdin.write(json.dumps({'jsonrpc': '2.0', 'id': self.sequence,
                                            'method': method, 'params': params}) + '\n')
        self.process.stdin.flush()
        reply = self.replies.get(timeout=40)
        assert reply['id'] == self.sequence, reply
        return reply

    def tool(self, name, arguments, denied=False):
        reply = self.request('tools/call', {'name': name, 'arguments': arguments})
        failed = 'error' in reply or reply.get('result', {}).get('isError')
        if denied:
            assert failed, reply
            return reply
        assert not failed, reply
        return json.loads(reply['result']['content'][0]['text'])

    def close(self):
        if self.process.poll() is None:
            self.process.stdin.close()
            self.process.wait(timeout=8)

