#!/usr/bin/env python3
"""Leiden community-detection sidecar — wraps igraph's community_leiden over
the length-prefixed JSON-RPC protocol used by crg-sidecar-bridge."""

import struct
import sys
import json


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


def run_leiden(edges, resolution, seed):
    import igraph as ig
    # Build vertex list from edges
    nodes = list({e['source'] for e in edges} | {e['target'] for e in edges})
    node_idx = {n: i for i, n in enumerate(nodes)}
    edge_list = [(node_idx[e['source']], node_idx[e['target']]) for e in edges]
    g = ig.Graph(n=len(nodes), edges=edge_list, directed=False)
    partition = g.community_leiden(
        objective_function='modularity',
        resolution_parameter=resolution,
        n_iterations=10,
        seed=seed,
    )
    return {nodes[i]: partition.membership[i] for i in range(len(nodes))}


def main():
    while True:
        req = read_message()
        method = req.get('method')
        params = req.get('params', {})
        try:
            if method == 'shutdown':
                write_message({'result': 'ok', 'error': None})
                break
            elif method == 'detect':
                result = run_leiden(
                    params['edges'],
                    params.get('resolution', 1.0),
                    params.get('seed', 42),
                )
                write_message({'result': {'communities': result}, 'error': None})
            else:
                write_message({'result': None, 'error': f'Unknown method: {method}'})
        except Exception as e:
            write_message({'result': None, 'error': str(e)})


if __name__ == '__main__':
    main()
