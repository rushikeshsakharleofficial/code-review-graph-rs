#!/usr/bin/env python3
"""Embeddings sidecar — serves sentence-transformers encode requests over the
length-prefixed JSON-RPC protocol used by crg-sidecar-bridge."""

import struct
import sys
import json
import os


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


model_cache = {}


def get_model(name):
    if name not in model_cache:
        from sentence_transformers import SentenceTransformer
        model_cache[name] = SentenceTransformer(name)
    return model_cache[name]


def main():
    while True:
        req = read_message()
        method = req.get('method')
        params = req.get('params', {})
        try:
            if method == 'shutdown':
                write_message({'result': 'ok', 'error': None})
                break
            elif method == 'encode':
                model = get_model(params.get('model', 'all-MiniLM-L6-v2'))
                texts = params['texts']
                vecs = model.encode(texts, normalize_embeddings=True)
                write_message({'result': {'vectors': vecs.tolist()}, 'error': None})
            else:
                write_message({'result': None, 'error': f'Unknown method: {method}'})
        except Exception as e:
            write_message({'result': None, 'error': str(e)})


if __name__ == '__main__':
    main()
