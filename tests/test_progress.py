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

PLAN = """
## Order of work

### Phase 1: make the tick fit

| ID | Task | Expected effect | Depends on |
|---|---|---|---|
| SRV-6 (#94) | jemalloc as the allocator | Faster | — |
| NET-9 (#95) | One hand-off per tick | Fewer wake-ups | — |

### Phase 2: fewer bytes

| ID | Task | Expected effect | Depends on |
|---|---|---|---|
| NET-10 | postcard for every payload | Smaller | — |

## What this plan does not change

| NET-99 | Not a task row of any phase | x |
"""


def issue(number, title, state="open", labels=(), reason=None, author="MEMBER", assignees=()):
    return {"number": number, "title": title, "state": state, "state_reason": reason, "author_association": author,
            "labels": [{"name": n} for n in labels], "assignees": [{"login": a} for a in assignees]}


def pr(number, title, draft=False, state="open", body="", author="MEMBER", login="maintainer"):
    return {"number": number, "title": title, "state": state, "draft": draft, "body": body, "pull_request": {},
            "author_association": author, "user": {"login": login}}


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
            issue(5, "IM-2: Filed by anyone", labels=("triage",), author="NONE"),
            issue(6, "NET-99: Closed by its author", state="closed", reason="completed", author="CONTRIBUTOR"),
        ]
        self.assertEqual(progress.collect(items, {}), [])

    def test_outsider_cannot_reuse_a_roadmap_id(self):
        items = [issue(1, "NET-1: Packet format", state="closed", reason="completed"),
                 issue(2, "NET-1: [spam](https://e.invalid)", author="NONE"),
                 issue(3, "NET-2: Closed by an outsider", state="closed", reason="completed", author="NONE")]
        tasks = by_id(progress.collect(items, progress.roadmap_tasks(ROADMAP)))
        self.assertEqual((tasks["NET-1"]["status"], tasks["NET-1"]["issue"]), ("done", 1))
        self.assertEqual((tasks["NET-2"]["status"], tasks["NET-2"]["issue"]), ("planned", None))

    def test_review_needs_a_member_or_the_assignee(self):
        task = issue(1, "NET-2: Connection", labels=("ready", "claimed"), assignees=("alice",))
        outsider = pr(2, "NET-2: Drive-by", body="Closes #1", author="NONE", login="mallory")
        self.assertEqual(progress.collect([task, outsider], {})[0]["status"], "in progress")
        claimer = pr(3, "NET-2: The work", body="Closes #1", author="CONTRIBUTOR", login="alice")
        self.assertEqual(progress.collect([task, outsider, claimer], {})[0]["status"], "in review")

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

    def test_plan_tasks(self):
        plan = progress.plan_tasks(PLAN)
        self.assertEqual(plan, {"SRV-6": (1, "jemalloc as the allocator"), "NET-9": (1, "One hand-off per tick"),
                                "NET-10": (2, "postcard for every payload")})
        self.assertEqual(list(plan), ["SRV-6", "NET-9", "NET-10"])  # the plan's order

    def test_steps_follow_the_plan_then_the_areas(self):
        plan = progress.plan_tasks(PLAN)
        items = [issue(n, t, labels=("ready",)) for n, t in
                 ((1, "NET-2: Two"), (2, "NET-9: Nine"), (3, "RP-1: Characters"), (4, "OPS-1: Gate"),
                  (5, "LAUNCH-1: Signing"), (6, "SRV-6: jemalloc"))]
        items[0] = issue(1, "NET-2: Two", state="closed", reason="completed")  # finished work outside the plan
        tasks = by_id(progress.collect(items, {}, plan))
        steps = progress.steps_for(plan)
        name = {t: (steps[tasks[t]["step"]]["name"] if tasks[t]["step"] is not None else None) for t in tasks}
        self.assertEqual(name["NET-2"], "Prototype and measurements")  # not in the plan
        self.assertEqual(name["NET-9"], "Make the tick fit")
        self.assertEqual(name["NET-10"], "Fewer bytes, then 30 Hz")  # from the plan, no issue yet
        self.assertEqual(name["RP-1"], "Roleplay base")
        self.assertEqual(name["LAUNCH-1"], "Launcher and releases")
        self.assertIsNone(name["OPS-1"])  # tooling track
        self.assertEqual([t["id"] for t in progress.collect(items, {}, plan)][:3], ["NET-2", "SRV-6", "NET-9"])

    def test_a_new_plan_phase_gets_a_step(self):
        plan = {"SRV-6": (1, "jemalloc"), "SRV-99": (9, "Later work")}
        steps = progress.steps_for(plan)
        names = [s["name"] for s in steps]
        self.assertEqual(names[names.index("Sync in the game") + 1], "Plan phase 9")
        self.assertEqual(steps[progress.collect([], {}, plan)[1]["step"]]["name"], "Plan phase 9")


def keys(tasks, plan=None):
    return [row[4] for row in progress.step_rows(tasks, plan)]


class Marker(unittest.TestCase):
    plan = {"SRV-6": (1, "jemalloc"), "NET-9": (1, "Hand-off"), "NET-10": (2, "postcard")}

    def tasks(self, *done):
        items = [issue(n, f"{i}: x", state="closed" if i in done else "open", reason="completed",
                       labels=("claimed",)) for n, i in enumerate(("NET-1", "SRV-6", "NET-9"), 1)]
        return progress.collect(items, {}, self.plan)

    def test_marker_follows_merged_work(self):
        # Step 2 is closed, so an open task outside the plan (NET-1 here) does not hold the marker there.
        self.assertEqual(keys(self.tasks(), self.plan)[:4], ["done", "done", "current", "todo"])
        self.assertEqual(keys(self.tasks("NET-1", "SRV-6"), self.plan)[:4], ["done", "done", "current", "todo"])
        # A task in review or in progress does not move the marker.
        self.assertEqual(keys(self.tasks("NET-1", "SRV-6", "NET-9"), self.plan)[:4],
                         ["done", "done", "done", "current"])

    def test_exactly_one_current_step(self):
        for done in ((), ("NET-1",), ("NET-1", "SRV-6", "NET-9")):
            self.assertEqual(keys(self.tasks(*done), self.plan).count("current"), 1)

    def test_task_outside_the_plan_does_not_reopen_a_finished_step(self):
        items = [issue(1, "NET-1: x", state="closed", reason="completed"),
                 issue(2, "SRV-6: x", labels=("claimed",)),
                 issue(3, "NET-14: Outside the plan, opened later", labels=("ready",))]
        tasks = by_id(progress.collect(items, {}, self.plan))
        self.assertIsNone(tasks["NET-14"]["step"])
        self.assertEqual(progress.track_of(tasks["NET-14"]), progress.OUTSIDE)
        self.assertEqual(tasks["NET-1"]["step"], 1)  # finished work stays in its step
        self.assertEqual(keys(list(tasks.values()), self.plan)[:3], ["done", "done", "current"])
        block = progress.render_markdown(list(tasks.values()), "o/r", self.plan)
        self.assertIn("We are here: step 3 of 11", block)
        self.assertIn("🟢 [NET-14](https://github.com/o/r/issues/3).", block)
        self.assertIn("| Outside the plan |", block)

    def test_every_step_is_described_with_a_difficulty(self):
        for step in progress.STEPS:
            self.assertIn(step["difficulty"], progress.DIFFICULTY, step["name"])
            self.assertTrue(step["why"] and step["covers"], step["name"])
        block = progress.render_markdown(self.tasks(), "o/r", self.plan)
        self.assertIn("| 🔴 **Very hard**<br><sub>", block)
        svg = progress.render_svg(self.tasks(), self.plan)
        self.assertEqual(svg.count('class="d"'), len(progress.STEPS))

    def test_every_step_links_to_its_roadmap_section(self):
        import re
        roadmap = open(os.path.join(progress.ROOT, progress.ROADMAP), encoding="utf-8").read()
        headings = {re.sub(r"[^a-z0-9 -]", "", h.lower()).replace(" ", "-")
                    for h in re.findall(r"^### (.+)$", roadmap, re.M)}
        for i, step in enumerate(progress.STEPS):
            self.assertIn(progress.roadmap_anchor(i, step), headings, step["name"])

    def test_waiting_step(self):
        rows = progress.step_rows(self.tasks(), self.plan)
        client = next(r for r in rows if r[0]["name"] == "Client module in the game")
        self.assertEqual((client[2], client[3], client[4]), (0, "waiting for the game", "waiting"))

    def test_weighted_fill(self):
        rows = progress.step_rows(self.tasks("NET-1", "SRV-6"), self.plan)
        self.assertEqual((rows[2][2], rows[2][3]), (50, "1 of 2 done"))


class Render(unittest.TestCase):
    def setUp(self):
        items = [issue(1, "NET-1: Packet | format", state="closed", reason="completed", labels=("ready",)),
                 issue(2, "NET-2: <Connection> & more", labels=("ready",))]
        self.tasks = progress.collect(items, {}, {"NET-2": (1, "Connection")})

    def test_markdown(self):
        block = progress.render_markdown(self.tasks, "o/r", {"NET-2": (1, "Connection")})
        self.assertTrue(block.startswith(progress.START) and block.endswith(progress.END))
        self.assertIn("[NET-1](https://github.com/o/r/issues/1): Packet \\| format", block)
        self.assertIn("We are here: step 3 of 11, make the tick fit", block)
        self.assertIn("0 of 1 done: 🟢 [NET-2](https://github.com/o/r/issues/2) |", block)
        self.assertEqual(block.count(progress.MARKER), 2)  # the sentence and the step's row

    def test_hostile_title_is_inert(self):
        title = ('[Download the client](https://example.invalid) ``` <img src="https://tracker.invalid/p.png"> '
                 '![x](https://e.invalid/i.png) www.example.invalid **bold** _x_ | <!-- comment')
        tasks = progress.collect([issue(1, f"NET-99: {title}", labels=("ready",), author="NONE")], {})
        block = progress.render_markdown(tasks, "o/r")
        row = next(line for line in block.splitlines() if "NET-99" in line and "): " in line)  # the task list
        for raw in ("](", "![", "<img", "<!--", "```", "https://e", "www.", "**", " | <", " @", "a@"):
            self.assertNotIn(raw, row.split("): ", 1)[1], raw)
        self.assertEqual(row.count(" | "), 4)  # still five cells

    def test_svg_is_well_formed(self):
        svg = progress.render_svg(self.tasks, {"NET-2": (1, "Connection")})
        xml.dom.minidom.parseString(svg)
        self.assertEqual(svg.count("WE ARE HERE"), 1)

    def test_splice(self):
        readme = f"head\n{progress.START}\nold\n{progress.END}\ntail\n"
        self.assertEqual(progress.splice(readme, "NEW"), "head\nNEW\ntail\n")
        with self.assertRaises(ValueError):
            progress.splice("no markers", "NEW")


if __name__ == "__main__":
    unittest.main()
