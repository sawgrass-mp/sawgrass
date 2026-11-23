"""Unit tests for scripts/progress.py. Standard library only, no network: python3 -I tests/test_progress.py"""
import os
import sys
import unittest
import xml.dom.minidom

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "scripts"))
import progress  # noqa: E402

ROADMAP = """
| ID | Task | Size |
|---|---|---|
| NET-1 | Packet format | S |
| NET-2 | Connection over QUIC | M |
| BOT-1 | Headless bot | M |
"""


def issue(number, title, state="open", labels=(), reason=None):
    return {"number": number, "title": title, "state": state, "state_reason": reason,
            "labels": [{"name": n} for n in labels]}


def pr(number, title, draft=False, state="open", body=""):
    return {"number": number, "title": title, "state": state, "draft": draft, "body": body, "pull_request": {}}


def by_id(tasks):
    return {t["id"]: t for t in tasks}


class Collect(unittest.TestCase):
    def test_statuses(self):
        items = [
            issue(1, "NET-1: Packet format", state="closed", reason="completed"),
            issue(2, "NET-2: Connection", labels=("ready", "claimed")),
            pr(3, "NET-2: Add the connection", body="Closes #2"),
            issue(4, "DB-1: Schema", labels=("ready", "claimed")),
            pr(7, "DB-1: Title only, no closing keyword"),
            issue(5, "SRV-1: Tick loop", labels=("ready",)),
        ]
        tasks = by_id(progress.collect(items, progress.roadmap_tasks(ROADMAP)))
        self.assertEqual(tasks["NET-1"]["status"], "done")
        self.assertEqual(tasks["NET-2"]["status"], "in review")
        self.assertEqual(tasks["DB-1"]["status"], "in progress")
        self.assertEqual(tasks["SRV-1"]["status"], "ready")
        # In the roadmap, no issue yet.
        self.assertEqual(tasks["BOT-1"]["status"], "planned")
        self.assertIsNone(tasks["BOT-1"]["issue"])

    def test_draft_pull_request_is_not_review(self):
        items = [issue(1, "NET-2: Connection", labels=("claimed",)), pr(2, "NET-2: WIP", draft=True, body="Closes #1")]
        self.assertEqual(progress.collect(items, {})[0]["status"], "in progress")

    def test_skipped_items(self):
        items = [
            issue(1, "NET-3: Dropped", state="closed", reason="not_planned"),
            issue(2, "Interest management: no task covers this"),
            issue(3, "SRV-1 follow-ups: metrics window"),
            pr(4, "OPS-9: Merged", state="closed"),
            issue(5, "IM-2: Filed by anyone", labels=("triage",)),
            issue(6, "NET-99: Closed by its author", state="closed", reason="completed"),
        ]
        self.assertEqual(progress.collect(items, {}), [])

    def test_open_issue_wins_over_closed_one(self):
        items = [issue(1, "NET-1: Old", state="closed", reason="completed"),
                 issue(2, "NET-1: Reopened work", labels=("ready",))]
        self.assertEqual(progress.collect(items, {})[0]["status"], "ready")

    def test_trusted_titles(self):
        tasks = by_id(progress.collect([issue(1, "NET-1: From the issue", labels=("ready",))],
                                       progress.roadmap_tasks(ROADMAP)))
        self.assertEqual(tasks["NET-1"]["title"], "From the issue")
        self.assertFalse(tasks["NET-1"]["trusted"])
        self.assertTrue(tasks["BOT-1"]["trusted"])  # roadmap only

    def test_sizes(self):
        items = [issue(1, "NET-1: Packet format", labels=("size: L",)), issue(2, "NET-2: Connection"),
                 issue(3, "SRV-7: Unknown size", labels=("ready",))]
        tasks = by_id(progress.collect(items, progress.roadmap_tasks(ROADMAP)))
        self.assertEqual(tasks["NET-1"]["size"], "L")  # the label wins over the roadmap
        self.assertEqual(tasks["NET-2"]["size"], "M")  # from the roadmap
        self.assertEqual(tasks["SRV-7"]["size"], progress.DEFAULT_SIZE)

    def test_order_follows_stages(self):
        items = [issue(n, t, labels=("ready",)) for n, t in
                 ((1, "DOC-1: Docs"), (2, "DB-1: Schema"), (3, "NET-10: Ten"), (4, "NET-2: Two"))]
        self.assertEqual([t["id"] for t in progress.collect(items, {})], ["NET-2", "NET-10", "DB-1", "DOC-1"])


class Numbers(unittest.TestCase):
    def test_weighted_progress(self):
        tasks = [{"id": "NET-1", "size": "S", "status": "done"}, {"id": "NET-2", "size": "M", "status": "ready"}]
        self.assertEqual(progress.progress(tasks), (1, 4, 25))
        self.assertEqual(progress.progress([]), (0, 0, 0))

    def test_bar(self):
        self.assertEqual(progress.bar(0), "░" * 20)
        self.assertEqual(progress.bar(50), "█" * 10 + "░" * 10)
        self.assertEqual(progress.bar(100), "█" * 20)

    def test_current_stage_skips_done_and_waiting(self):
        done = [{"id": "NET-1", "size": "S", "status": "done"}]
        self.assertEqual(progress.current_stage(done)["n"], 2)
        self.assertEqual(progress.current_stage(done + [{"id": "BOT-1", "size": "M", "status": "ready"}])["n"], 1)


class Render(unittest.TestCase):
    def setUp(self):
        items = [issue(1, "NET-1: Packet | format", state="closed", reason="completed", labels=("ready",)),
                 issue(2, "NET-2: <Connection> & more", labels=("ready",))]
        self.tasks = progress.collect(items, {})

    def test_markdown(self):
        block = progress.render_markdown(self.tasks, "o/r")
        self.assertTrue(block.startswith(progress.START) and block.endswith(progress.END))
        self.assertIn("[NET-1](https://github.com/o/r/issues/1): Packet \\| format", block)
        self.assertIn("Current stage: 1", block)

    def test_hostile_title_is_inert(self):
        title = ('[Download the client](https://example.invalid) ``` <img src="https://tracker.invalid/p.png"> '
                 '![x](https://e.invalid/i.png) www.example.invalid **bold** _x_ | <!-- comment')
        tasks = progress.collect([issue(1, f"NET-99: {title}", labels=("ready",))], {})
        block = progress.render_markdown(tasks, "o/r")
        row = next(line for line in block.splitlines() if "NET-99" in line)
        for raw in ("](", "![", "<img", "<!--", "```", "https://e", "www.", "**", " | <"):
            self.assertNotIn(raw, row.split("): ", 1)[1], raw)
        self.assertEqual(row.count(" | "), 3)  # still four cells

    def test_svg_is_well_formed(self):
        svg = progress.render_svg(self.tasks)
        xml.dom.minidom.parseString(svg)

    def test_splice(self):
        readme = f"head\n{progress.START}\nold\n{progress.END}\ntail\n"
        self.assertEqual(progress.splice(readme, "NEW"), "head\nNEW\ntail\n")
        with self.assertRaises(ValueError):
            progress.splice("no markers", "NEW")


if __name__ == "__main__":
    unittest.main()
