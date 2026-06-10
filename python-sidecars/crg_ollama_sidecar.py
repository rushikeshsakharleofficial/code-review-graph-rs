#!/usr/bin/env python3
"""Ollama generation sidecar — proxies text-generation requests to a local
Ollama server over the length-prefixed JSON-RPC protocol used by
crg-sidecar-bridge."""

import struct
import sys
import json
import urllib.request


def read_message():
    raw = sys.stdin.buffer.read(4)
    if len(raw) < 4:
        sys.exit(0)
    length = struct.unpack('<I', raw)[0]
    return json.loads(sys.stdin.buffer.read(length))


def write_message(obj):
    data = json.dumps(obj).encode('utf-8')
    sys.stdout.buffer.write(struct.pack('<I', len(data)))
    sys.stdout.buffer.write(data)
    sys.stdout.buffer.flush()


def ollama_generate(prompt, model, stream=False):
    payload = json.dumps({"model": model, "prompt": prompt, "stream": stream}).encode()
    req = urllib.request.Request(
        "http://localhost:11434/api/generate",
        data=payload,
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    with urllib.request.urlopen(req, timeout=120) as resp:
        data = json.loads(resp.read())
        return data.get("response", "")


def main():
    while True:
        req = read_message()
        method = req.get('method')
        params = req.get('params', {})
        try:
            if method == 'shutdown':
                write_message({'result': 'ok', 'error': None})
                break
            elif method == 'generate':
                text = ollama_generate(
                    params['prompt'],
                    params.get('model', 'llama3'),
                    params.get('stream', False),
                )
                write_message({'result': {'text': text}, 'error': None})
            else:
                write_message({'result': None, 'error': f'Unknown method: {method}'})
        except Exception as e:
            write_message({'result': None, 'error': str(e)})


if __name__ == '__main__':
    main()
