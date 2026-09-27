#!/usr/bin/env python3
"""Report upstream changes since the last explicitly reviewed snapshot."""
import argparse
import json
import os
from pathlib import Path
import urllib.request

REPO = "truce-audio/truce"
ROOT = Path(__file__).resolve().parents[1]
BASELINE = ROOT / "docs/upstream/truce.json"


def api(path):
    headers = {"Accept": "application/vnd.github+json", "User-Agent": "moose-upstream-tracker"}
    token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")
    if token:
        headers["Authorization"] = f"Bearer {token}"
    request = urllib.request.Request(f"https://api.github.com/repos/{REPO}/{path}", headers=headers)
    with urllib.request.urlopen(request, timeout=30) as response:
        return json.load(response)


def snapshot():
    items = {}
    page = 1
    while True:
        batch = api(f"issues?state=all&per_page=100&page={page}")
        for item in batch:
            items[str(item["number"])] = {
                key: item[key] for key in ("title", "state", "updated_at", "html_url")
            }
        if len(batch) < 100:
            break
        page += 1
    return {"repository": REPO, "head": api("commits/main")["sha"], "items": items}


def changes(old, new):
    return [item for number, item in new["items"].items() if old["items"].get(number) != item]


def report(old, new):
    updated = changes(old, new)
    lines = ["# Truce upstream review", "", f"Baseline: `{old['head']}`", f"Current: `{new['head']}`", ""]
    if old["head"] != new["head"]:
        lines += [f"[Review commits](https://github.com/{REPO}/compare/{old['head']}...{new['head']})", ""]
    for item in updated:
        # Titles are untrusted Markdown; keep each entry on one line.
        title = item["title"].replace("\n", " ").replace("[", "\\[").replace("]", "\\]")
        lines.append(f"- [{title}]({item['html_url']}) — {item['state']}, updated {item['updated_at']}")
    pending = bool(updated) or old["head"] != new["head"]
    lines += ["", "Review required; acknowledge with `python3 scripts/truce-upstream.py --accept` after triage." if pending else "No unreviewed upstream changes."]
    return "\n".join(lines) + "\n", pending


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--accept", action="store_true", help="Record today's upstream state as reviewed; commit the resulting baseline")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        old = {"head": "a", "items": {}}
        assert report(old, old)[1] is False
        assert report(old, {"head": "b", "items": {}})[1] is True
        item = {"title": "new", "state": "open", "updated_at": "now", "html_url": "https://github.com/truce-audio/truce/issues/1"}
        new = {"head": "a", "items": {"1": item}}
        assert changes(old, new) == [item]
        assert report(new, new)[1] is False
        changed = {"head": "a", "items": {"1": dict(item, state="closed")}}
        assert len(changes(new, changed)) == 1
        return 0
    current = snapshot()
    if args.accept:
        BASELINE.write_text(json.dumps(current, indent=2, sort_keys=True) + "\n")
        print(f"Recorded {len(current['items'])} issues/PRs at {current['head']} in {BASELINE}")
        return 0
    text, pending = report(json.loads(BASELINE.read_text()), current)
    print(text)
    if summary := os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(summary, "a") as out:
            out.write(text)
    # Scheduled workflow failure notifies its subscribers without posting comments
    # or trusting upstream content as code. Normal build CI never runs this gate.
    return 1 if pending else 0


if __name__ == "__main__":
    raise SystemExit(main())
