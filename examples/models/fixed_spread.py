#!/usr/bin/env python3
"""A fixed spread and a fixed size, with no regard for the inventory.

This is the second pricing model the engine is run with (SC-011): a model of
"a firm's own" that shares nothing with the built-in one but the protocol. It
quotes the feed's mid with the same half-spread on every tick, never shifts
the mid, and offers the same size — so a quote it priced is recognisable on
chain at a glance (`spread_bps` = the configured one, `skew_bps` = 0), and
the end-to-end run can tell which model was quoting.

It is deliberately not a good market maker: it does not steer the inventory
back, and the vault's hard skew bound (FR-026) is what stops it. Use
`spread_skew.py` as the starting point for a real model.

Run by the engine (`MODEL_COMMAND` in `.env`):

    python3 examples/models/fixed_spread.py --spread-bps 25 --size-base 1000000000

The protocol is v1, the same as in `spread_skew.py`: one JSON question per
line on stdin, one answer per line on stdout carrying the question's `id`;
`u64`/`u128` values as decimal strings; stdout is the protocol, logs go to
stderr.
"""

import argparse
import json
import sys

PROTOCOL_VERSION = 1
BPS_DENOM = 10_000
U64_MAX = 2**64 - 1


def answer(question, spread_bps, size_base):
    if question.get("v") != PROTOCOL_VERSION:
        raise ValueError(f"protocol version {question.get('v')!r}, this model speaks {PROTOCOL_VERSION}")
    mid_e9 = int(question["mid_e9"])
    if mid_e9 == 0:
        return {"withdraw": "no mid to quote around"}
    return {
        "quote": {
            "mid_e9": str(mid_e9),
            "spread_bps": spread_bps,
            "skew_bps": 0,
            "max_size_base": str(size_base),
        }
    }


def serve(spread_bps, size_base, stdin=sys.stdin, stdout=sys.stdout, stderr=sys.stderr):
    for line in stdin:
        if not line.strip():
            continue
        try:
            question = json.loads(line)
        except ValueError as error:
            print(f"not a question: {error}", file=stderr, flush=True)
            continue
        try:
            reply = {"id": question["id"], **answer(question, spread_bps, size_base)}
        except Exception as error:  # noqa: BLE001 — every failure is an `error` answer
            reply = {"id": question.get("id"), "error": f"{type(error).__name__}: {error}"}
        stdout.write(json.dumps(reply, separators=(",", ":")) + "\n")
        stdout.flush()


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--spread-bps", type=int, required=True)
    parser.add_argument("--size-base", type=int, required=True, help="raw units of the base asset")
    args = parser.parse_args()
    if not 1 <= args.spread_bps < BPS_DENOM:
        parser.exit(2, f"fixed_spread.py: a half-spread of {args.spread_bps} bps is not a spread\n")
    if not 1 <= args.size_base <= U64_MAX:
        parser.exit(2, f"fixed_spread.py: a size of {args.size_base} is not a u64 above zero\n")
    print(f"fixed_spread.py ready, protocol v{PROTOCOL_VERSION}", file=sys.stderr, flush=True)
    serve(args.spread_bps, args.size_base)


if __name__ == "__main__":
    main()
