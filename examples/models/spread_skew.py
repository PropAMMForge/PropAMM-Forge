#!/usr/bin/env python3
"""The built-in spread-and-skew model, in Python, over the model protocol v1.

This is the reference for writing a pricing model outside the Rust core
(FR-015a). It is the engine's own `SpreadSkewModel` ported line by line, and a
test in `crates/propamm-engine` asks both the same questions and requires the
same answers — so it is also a check that the protocol carries everything a
model needs.

Run by the engine as a child process:

    python3 examples/models/spread_skew.py \\
        --base-spread-bps 10 --max-spread-bps 40 \\
        --max-skew-shift-bps 150 --size-fraction-bps 500

The protocol, in short (the full description is in
`crates/propamm-engine/src/model/external.rs`):

- one JSON object per line on stdin — a question; one per line on stdout — the
  answer, carrying the question's `id` and exactly one of `quote`, `withdraw`,
  `error`;
- `u64`/`u128` values are decimal strings, basis points are numbers;
- stdout is the protocol and nothing else — log to stderr.

To write your own model, keep `serve` and replace `SpreadSkewModel.quote`. Stay
on integers: the program prices in integer arithmetic, and a float mid is a
mid that differs from what the router will reproduce.
"""

import argparse
import json
import sys

PROTOCOL_VERSION = 1
BPS_DENOM = 10_000
PRICE_SCALE = 1_000_000_000
U64_MAX = 2**64 - 1


def div_trunc(a, b):
    """Integer division rounding toward zero, as Rust's `/` does.

    Python's `//` rounds toward negative infinity: on a negative skew the two
    differ by one, and the port would stop agreeing with the Rust model.
    """
    q = abs(a) // abs(b)
    return q if (a >= 0) == (b > 0) else -q


def inventory_skew_bps(base_amount, quote_amount, mid_e9):
    """Positive — too much base, negative — too much quote, both at the mid."""
    base_value = base_amount * mid_e9
    quote_value = quote_amount * PRICE_SCALE
    total = base_value + quote_value
    if total == 0:
        return 0
    return div_trunc((base_value - quote_value) * BPS_DENOM, total)


class SpreadSkewModel:
    def __init__(self, base_spread_bps, max_spread_bps, max_skew_shift_bps, size_fraction_bps):
        if max_spread_bps >= BPS_DENOM:
            raise ValueError(f"a half-spread of {max_spread_bps} bps is not a spread")
        if base_spread_bps > max_spread_bps:
            raise ValueError(f"the base half-spread of {base_spread_bps} bps is wider than the maximum")
        if max_skew_shift_bps >= BPS_DENOM:
            raise ValueError(f"a mid shift of {max_skew_shift_bps} bps is out of range")
        if max_skew_shift_bps <= max_spread_bps - base_spread_bps:
            # The steering has to beat the brake: see the Rust model's docs.
            raise ValueError("the mid shift does not exceed the widening; the skew would never come back")
        if not 1 <= size_fraction_bps <= BPS_DENOM:
            raise ValueError(f"the size fraction must be between 1 and 10 000 bps, not {size_fraction_bps}")
        self.base_spread_bps = base_spread_bps
        self.max_spread_bps = max_spread_bps
        self.max_skew_shift_bps = max_skew_shift_bps
        self.size_fraction_bps = size_fraction_bps

    def quote(self, mid_e9, base_amount, quote_amount, max_skew_bps):
        if mid_e9 == 0:
            raise ValueError("quote is not set")
        if max_skew_bps == 0:
            raise ValueError("quote parameters are out of domain")

        skew_bps = inventory_skew_bps(base_amount, quote_amount, mid_e9)
        load = min(abs(skew_bps), max_skew_bps) * BPS_DENOM // max_skew_bps

        widening = self.max_spread_bps - self.base_spread_bps
        spread = self.base_spread_bps + widening * load // BPS_DENOM
        shift = self.max_skew_shift_bps * load // BPS_DENOM
        if skew_bps > 0:
            shift = -shift

        quote_as_base = quote_amount * PRICE_SCALE // mid_e9
        payable = min(base_amount, quote_as_base)
        size = min(payable * self.size_fraction_bps // BPS_DENOM, U64_MAX)

        return {
            "mid_e9": str(mid_e9),
            "spread_bps": spread,
            "skew_bps": shift,
            "max_size_base": str(size),
        }


def answer(question, model):
    if question.get("v") != PROTOCOL_VERSION:
        raise ValueError(f"protocol version {question.get('v')!r}, this model speaks {PROTOCOL_VERSION}")
    inventory = question["inventory"]
    return model.quote(
        int(question["mid_e9"]),
        int(inventory["base_amount"]),
        int(inventory["quote_amount"]),
        int(question["max_skew_bps"]),
    )


def serve(model, stdin=sys.stdin, stdout=sys.stdout, stderr=sys.stderr):
    for line in stdin:
        if not line.strip():
            continue
        try:
            question = json.loads(line)
        except ValueError as error:
            # No id to answer to; the engine never sends this.
            print(f"not a question: {error}", file=stderr, flush=True)
            continue
        try:
            reply = {"id": question["id"], "quote": answer(question, model)}
        except Exception as error:  # noqa: BLE001 — every failure is an `error` answer
            reply = {"id": question.get("id"), "error": f"{type(error).__name__}: {error}"}
        stdout.write(json.dumps(reply, separators=(",", ":")) + "\n")
        stdout.flush()


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--base-spread-bps", type=int, required=True)
    parser.add_argument("--max-spread-bps", type=int, required=True)
    parser.add_argument("--max-skew-shift-bps", type=int, required=True)
    parser.add_argument("--size-fraction-bps", type=int, required=True)
    args = parser.parse_args()
    try:
        model = SpreadSkewModel(
            args.base_spread_bps,
            args.max_spread_bps,
            args.max_skew_shift_bps,
            args.size_fraction_bps,
        )
    except ValueError as error:
        parser.exit(2, f"spread_skew.py: {error}\n")
    print(f"spread_skew.py ready, protocol v{PROTOCOL_VERSION}", file=sys.stderr, flush=True)
    serve(model)


if __name__ == "__main__":
    main()
