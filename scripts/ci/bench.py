#!/usr/bin/env python3
"""Bench tests: what a headless machine could not test (AGENTS.md, "Bench tests").

Most development runs on servers with no sound card, MIDI device, display or
listener. Every PR body says what is left to check on a machine that has
them, under a `## Bench test` heading:

    ## Bench test
    Needs: midi, human
    1. Plug in a wind controller, load the Wind template, blow a note.
    Expect: the filter opens with breath; the note ends when you stop.

or `none: <reason>` when nothing is left. When the PR merges, `open` turns
the section into one issue labelled `bench`; a bench session on a machine
with the devices works that queue. Open bench issues hold back a release.

usage:
  scripts/ci/bench.py check [--pr N]        fail on a missing or malformed section
                                            (without --pr: PR body on stdin)
  scripts/ci/bench.py open --pr N [--dry-run]
                                            open the bench issue of a merged PR
  scripts/ci/bench.py gate                  fail while bench issues are open
  scripts/ci/bench.py --selftest
`--pr`, `open` and `gate` call `gh` (GH_TOKEN, and GH_REPO outside a checkout).
"""
import contextlib
import io
import json
import re
import subprocess
import sys

QUEUE = "bench"
FAILED = "bench-failed"
NEEDS = {
    "audio": "a real sound card or audio interface",
    "midi": "a hardware MIDI device",
    "display": "the real app in a desktop session",
    "human": "a person who listens, plays or looks",
}
LABELS = {
    QUEUE: ("1D76DB", "Waits for a test on a machine with the devices (bench.yml)"),
    FAILED: ("B60205", "Bug found by a bench test; holds back a release"),
    **{f"{QUEUE}:{need}": ("BFD4F2", f"Bench test needs {what}") for need, what in NEEDS.items()},
}
AREAS = {"engine", "ui", "agent", "plugins", "midi", "mixing", "testing"}
HEADING = re.compile(r"^##\s+Bench test\s*$", re.I | re.M)
NEXT_HEADING = re.compile(r"^#{1,2}\s", re.M)
NONE = re.compile(r"none\b\s*[:—–-]\s*(\S.*)", re.I | re.S)
NEEDS_LINE = re.compile(r"^Needs:(.*)$", re.I | re.M)
FOOTER = re.compile(r"^🤖 Generated with.*$", re.M)
EXAMPLE = "`none: <reason>`, or `Needs: " + ", ".join(NEEDS) + "` (those that apply) followed by the steps"


def die(msg):
    print(f"bench.py: {msg}", file=sys.stderr)
    sys.exit(1)


def parse(body):
    """What a PR body leaves for the bench: {"kind": "none" | "test" | "invalid", ...}."""
    text = re.sub(r"<!--.*?-->", "", (body or "").replace("\r\n", "\n"), flags=re.S)
    heading = HEADING.search(text)
    if not heading:
        return {"kind": "invalid", "problem": "no `## Bench test` section"}
    section = text[heading.end():]
    following = NEXT_HEADING.search(section)
    if following:
        section = section[: following.start()]
    section = FOOTER.sub("", section).strip()
    if not section:
        return {"kind": "invalid", "problem": "the `## Bench test` section is empty"}
    none = NONE.fullmatch(section)
    if none:
        return {"kind": "none", "reason": none.group(1).strip()}
    if re.match(r"none\b", section, re.I):
        return {"kind": "invalid", "problem": "`none` needs a reason: `none: <reason>`"}
    needs_line = NEEDS_LINE.search(section)
    if not needs_line:
        return {"kind": "invalid", "problem": "no `Needs:` line"}
    needs = [n for n in re.split(r"[,\s]+", needs_line.group(1).strip().lower()) if n]
    unknown = [n for n in needs if n not in NEEDS]
    if unknown or not needs:
        named = ", ".join(unknown) or "nothing"
        return {"kind": "invalid", "problem": f"`Needs:` names {named}; known: {', '.join(NEEDS)}"}
    steps = (section[: needs_line.start()] + section[needs_line.end():]).strip()
    if not steps:
        return {"kind": "invalid", "problem": "no steps after `Needs:`"}
    return {"kind": "test", "needs": sorted(set(needs), key=list(NEEDS).index), "steps": steps}


def exempt(pr):
    """Release PRs and bot PRs change no behaviour a bench could test."""
    if pr["author"].get("is_bot") or pr["author"]["login"].endswith("[bot]"):
        return f"opened by {pr['author']['login']}"
    if pr["headRefName"].startswith("release-"):
        return "a release PR"
    return None


def issue_title(pr):
    return f"Bench test for #{pr['number']}: {pr['title']}"


def issue_for(pr):
    """The issue a merged PR leaves behind: (title, labels, body), or None."""
    verdict = parse(pr["body"])
    if verdict["kind"] == "none":
        return None
    sha = (pr.get("mergeCommit") or {}).get("oid") or "unknown"
    areas = sorted(AREAS & {label["name"] for label in pr["labels"]})
    head = f"#{pr['number']} merged as {sha} without this check on real hardware."
    if verdict["kind"] == "test":
        labels = [QUEUE] + [f"{QUEUE}:{need}" for need in verdict["needs"]] + areas
        body = f"""{head}

**Needs:** {", ".join(f"{need} ({NEEDS[need]})" for need in verdict["needs"])}

{verdict["steps"]}

---
For a bench session (`.claude/skills/bench-session/SKILL.md`): test `main` at
or after `{sha[:9]}`, post what you ran and saw as a comment, and close this
issue on a pass. On a fail, open a bug labelled `{FAILED}`, link it here and
close this issue.
"""
    else:
        labels = [QUEUE] + areas
        body = f"""{head}

Its description does not say what is left to test: {verdict["problem"]}.

Any agent can settle this from the PR's diff: either close this issue with
the reason nothing needs real hardware, or replace this text with the steps
and add the `{QUEUE}:<need>` labels ({", ".join(NEEDS)}).
"""
    return issue_title(pr), labels, body


def gh(*args, stdin=None):
    done = subprocess.run(["gh", *args], input=stdin, text=True, capture_output=True)
    if done.returncode != 0:
        die(f"gh {' '.join(args[:3])} failed: {done.stderr.strip()}")
    return done.stdout


def fetch_pr(number):
    fields = "number,title,body,labels,author,headRefName,mergeCommit,state,url"
    return json.loads(gh("pr", "view", str(number), "--json", fields))


def check(pr):
    skip = pr.get("author") and exempt(pr)
    if skip:
        print(f"no bench section needed ({skip}) — ok")
        return 0
    verdict = parse(pr["body"])
    if verdict["kind"] == "invalid":
        print(f"::error::{verdict['problem']}. Every PR says what is left to test on real "
              f"hardware (AGENTS.md, \"Bench tests\"): under `## Bench test` write {EXAMPLE}.")
        return 1
    if verdict["kind"] == "none":
        print(f"nothing left for the bench ({verdict['reason']}) — ok")
    else:
        print(f"bench test declared (needs {', '.join(verdict['needs'])}) — ok")
    return 0


def open_issue(number, dry_run):
    pr = fetch_pr(number)
    if pr["state"] != "MERGED":
        die(f"#{number} is {pr['state']}, not merged")
    skip = exempt(pr)
    issue = None if skip else issue_for(pr)
    if not issue:
        print(f"#{number}: no bench issue ({skip or 'nothing left for the bench'})")
        return
    title, labels, body = issue
    listed = gh("issue", "list", "--state", "all", "--label", QUEUE, "--limit", "1000",
                "--json", "number,title")
    known = [i["number"] for i in json.loads(listed)
             if i["title"].startswith(f"Bench test for #{number}:")]
    if known:
        print(f"#{number}: bench issue #{known[0]} exists")
        return
    if dry_run:
        print(f"would open: {title}\nlabels: {', '.join(labels)}\n\n{body}")
        return
    for name in labels:
        if name in LABELS:
            color, description = LABELS[name]
            gh("label", "create", name, "--color", color, "--description", description, "--force")
    create = ["issue", "create", "--title", title, "--body-file", "-"]
    for name in labels:
        create += ["--label", name]
    print(gh(*create, stdin=body).strip())


def gate():
    waiting = {}
    for label in (QUEUE, FAILED):
        listed = gh("issue", "list", "--state", "open", "--label", label, "--limit", "1000",
                    "--json", "number,title")
        for issue in json.loads(listed):
            waiting[issue["number"]] = issue["title"]
    for number, title in sorted(waiting.items()):
        print(f"  #{number} {title}")
    if waiting:
        print(f"::error::{len(waiting)} bench issue(s) open: merged changes not yet tested on "
              "real hardware. Run a bench session, or release anyway with skip_bench=true.")
        return 1
    print("no open bench issues — ok")
    return 0


# Real PR bodies (`gh pr view <n> --json body`), trimmed (AGENTS.md rule 63).
# #138 merged saying "Not tested with a physical wind controller (none
# attached)" in its evidence, and nobody was told: the case this exists for.
PR138 = """## Why
Wind synthesizers should be first-class in kabelsalat.

## Evidence
`kabelsalat wind-test` (real Vitalium, ElectrowindsVol1 bank imported locally):

Not tested with a physical wind controller (none attached). The decoding is unit-tested on raw bytes.

Touches `AGENTS.md`, so the human gate holds it.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
"""
WIND = """
## Bench test
<!-- What is left to test on real hardware? -->
Needs: midi, human
1. Plug in a wind controller and load a Vital wind patch.
2. Blow a note.

Expect: the filter opens with breath.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
"""


def selftest():
    def pr(body, **over):
        base = {"number": 138, "title": "Breath from wind controllers", "body": body,
                "labels": [{"name": "midi"}, {"name": "enhancement"}],
                "author": {"login": "ssalentin", "is_bot": False},
                "headRefName": "wind-breath-through",
                "mergeCommit": {"oid": "8ea336c5ba7428957b57c6b150e80b1f85fd6854"}}
        return {**base, **over}

    wind = PR138.replace("\n🤖", WIND.rsplit("🤖", 1)[0] + "🤖", 1)
    title, labels, body = issue_for(pr(wind))
    untold = issue_for(pr(PR138))
    with contextlib.redirect_stdout(io.StringIO()) as said:
        refused = check({"body": PR138})
    cases = [
        ("#138 as merged has no section", parse(PR138)["kind"] == "invalid"),
        ("#138 as merged fails the check, and the error says what to write",
         refused == 1 and "::error::" in said.getvalue() and "none: <reason>" in said.getvalue()),
        ("#138 as merged still leaves an issue to settle",
         untold[1] == ["bench", "midi"] and "no `## Bench test` section" in untold[2]),
        ("steps and needs are read", parse(wind)["needs"] == ["midi", "human"]),
        ("the comment and the footer stay out of the steps",
         "What is left" not in body and "Generated with" not in body
         and "Expect: the filter opens with breath." in body),
        ("the issue names the PR and the merge commit",
         title == "Bench test for #138: Breath from wind controllers" and "8ea336c5b" in body),
        ("needs and the PR's area become labels",
         labels == ["bench", "bench:midi", "bench:human", "midi"]),
        ("a section in the middle ends at the next heading",
         parse("## Bench test\nNeeds: audio\nplay\n\n## Verification\nran it")["steps"] == "play"),
        ("CRLF bodies parse (the web editor sends them)",
         parse("## Bench test\r\nnone: docs only\r\n")["reason"] == "docs only"),
        ("none with a reason leaves no issue", issue_for(pr("## Bench test\nnone: docs only")) is None),
        ("none without a reason is refused", parse("## Bench test\nnone")["kind"] == "invalid"),
        ("an untouched template is refused",
         parse("## Bench test\n\n<!-- fill me in -->\n\n## Evidence\nx")["kind"] == "invalid"),
        ("an unknown need is refused",
         "ears" in parse("## Bench test\nNeeds: ears\nlisten")["problem"]),
        ("needs without steps are refused", parse("## Bench test\nNeeds: audio\n")["kind"] == "invalid"),
        ("a release PR is exempt", exempt(pr("", headRefName="release-0.9.0")) is not None),
        ("a bot's PR is exempt",
         exempt(pr("", author={"login": "github-actions[bot]", "is_bot": True})) is not None),
        ("a person's PR is not exempt", exempt(pr("")) is None),
    ]
    failed = [name for name, ok in cases if not ok]
    for name, ok in cases:
        print(f"  {'ok' if ok else 'FAIL'}: {name}")
    print(f"selftest: {len(cases) - len(failed)} ok, {len(failed)} failed")
    return 1 if failed else 0


def main(argv):
    def pr_number():
        if "--pr" not in argv or argv.index("--pr") + 1 == len(argv):
            return None
        return int(argv[argv.index("--pr") + 1])

    command = argv[0] if argv else ""
    if command == "--selftest":
        return selftest()
    if command == "check":
        number = pr_number()
        return check(fetch_pr(number) if number else {"body": sys.stdin.read()})
    if command == "open" and pr_number():
        return open_issue(pr_number(), "--dry-run" in argv)
    if command == "gate":
        return gate()
    die(__doc__[__doc__.index("usage:"):].strip())


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
