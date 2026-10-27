"""python3 scripts/twin/test_twin_check.py  (standard library only)"""
import contextlib
import io
import json
import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import twin_check  # noqa: E402


def engine(mode="peers", observe=True, refused=0, differ=0, seq=107_950_100):
    return {
        "ledger_source": {"mode": mode, "from_peers": 500, "holds": 2, "keyed_fallbacks": 74, "chain_walks": 90, "rpc_refused": refused},
        "own_hash": {"ledgers": 500, "own": 426, "differ": differ, "streak": 420, "last_seq": seq},
        "observe": {"observe": observe, "would_sign": 420, "would_sign_own": 420},
    }


def run(twin, live, state=None):
    canned = {"t/api/engine": twin, "l/api/engine": live}
    args = ["--twin", "t", "--live", "l"] + (["--state", state] if state else [])
    out = io.StringIO()
    with contextlib.redirect_stdout(out):
        rc = twin_check.main(args, fetch=lambda u: canned[u])
    return rc, out.getvalue()


class TwinCheck(unittest.TestCase):
    def test_a_healthy_twin_passes(self):
        rc, out = run(engine(), engine(seq=107_950_102))
        self.assertEqual(rc, 0)
        self.assertTrue(out.startswith("TWIN OK: twin #107950100 (live #107950102, lag 2)"), out)

    def test_a_refused_rpc_call_fails_and_is_named(self):
        rc, out = run(engine(refused=3), engine())
        self.assertEqual(rc, 1)
        self.assertTrue(out.startswith("TWIN PROBLEM:") and "rpc_refused 3: a path still asks .39" in out, out)

    def test_not_observing_or_not_on_peers_fails(self):
        rc, out = run(engine(observe=False), engine())
        self.assertEqual(rc, 1)
        self.assertIn("not observing", out)
        rc, out = run(engine(mode="rpc"), engine())
        self.assertEqual(rc, 1)
        self.assertIn("not on the peer source", out)

    def test_lag_and_new_differs_fail_but_old_ones_do_not(self):
        with tempfile.TemporaryDirectory() as d:
            state = os.path.join(d, "twin.json")
            rc, out = run(engine(differ=1), engine(), state)
            self.assertEqual(rc, 1)
            self.assertIn("differed on 1 more", out)
            with open(state) as f:
                self.assertEqual(json.load(f), {"differ": 1})
            rc, _ = run(engine(differ=1), engine(), state)
            self.assertEqual(rc, 0, "the same receipt is not news")
            rc, out = run(engine(seq=100), engine(seq=200), state)
            self.assertEqual(rc, 1)
            self.assertIn("100 ledgers behind", out)


if __name__ == "__main__":
    unittest.main()
