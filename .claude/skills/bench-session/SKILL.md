---
name: bench-session
description: Works the queue of bench tests on a machine with real audio or MIDI hardware, a desktop session or the user at hand. Bench tests are GitHub issues labelled `bench`, opened by bench.yml for merged PRs that a headless server could not fully test. Use on the MacBook (or any machine whose agents.local.md has a `bench:` line) when the user says "bench session", "run the bench tests", "test what the server could not", names a device they just plugged in, or before a release that open bench issues hold back.
---

# Bench session

**Goal:** every merged change is tested on real hardware before it ships.
Headless servers merge on green CI and leave what they could not test as
issues labelled `bench` (AGENTS.md, "Bench tests"). This session runs those
tests and records what happened. `prepare-release.yml` refuses to release
while one is open.

## 1. What can this machine test right now?

Read `~/.config/kabelsalat/agents.local.md`. Its `bench:` line lists what
the machine has in principle (`audio`, `midi`, `display`, `human`); without
that line this is not a bench machine, so stop and say so. Then check what
is connected now, because a need is only met while its device is plugged in:

```sh
pactl list short sinks; pactl list short sources   # audio
aconnect -l                                        # MIDI ports
echo "$WAYLAND_DISPLAY"                            # display
```

`human` holds only while the user is in the session and has said they have
time to listen and play.

## 2. Read the queue

```sh
gh issue list --label bench --state open --limit 200 \
  --json number,title,labels,body
```

Sort the issues into three groups and tell the user the plan before you
start:

- **Runnable now:** every `bench:<need>` label is met, and the device the
  steps name is connected.
- **Needs a device:** group by device, so the user plugs each one in once
  ("plug in the MIDIMATE to clear #151, #154").
- **Not ready:** an issue with no `bench:<need>` label has no steps yet (the
  PR merged without a bench section). Read the PR's diff and settle it: close
  it with the reason nothing needs real hardware, or write the steps into
  the issue, add the labels and run it.

## 3. One build for the whole queue

```sh
git fetch origin
git worktree add ../kabelsalat-bench-run origin/main --detach
cd ../kabelsalat-bench-run && cargo build
git rev-parse HEAD
```

Each issue names the merge commit it tests. `main` contains all of them, so
one build serves every issue; check with
`git merge-base --is-ancestor <sha> HEAD` and skip an issue whose commit is
missing. Remove the worktree when the session ends.

## 4. Run each test

Follow the issue's steps as written, and compare with its `Expect:`. If a
step is unclear or cannot be done as written, do not guess: comment what is
missing and leave the issue open.

Do the tests agents can run alone first. Collect the `human` ones and go
through them with the user in one sitting: set everything up (build, launch,
session loaded, device selected), say exactly what to do and what to listen
for, then record the user's answer in their words.

## 5. Record the result

Comment on the issue every time, pass or fail:

- the commit tested (`git rev-parse HEAD`) and the machine,
- the devices used, by the name the system shows,
- each command and its output (trimmed to the lines that decide it), numbers
  with units, screenshots by path when the test is visual,
- for `human` tests, what the user said.

Then:

- **Pass:** `gh issue close <n> --reason completed`.
- **Fail:** open a bug with the comment's content and a reference to the PR
  and the bench issue, labelled `bug`, `bench-failed` and the area labels.
  Link it in the bench issue and close the bench issue with
  `--reason "not planned"`. The fix is ordinary work for any agent; its PR
  carries its own `## Bench test` section with the same steps, so the retest
  arrives here as a new issue. `bench-failed` keeps the release held until
  the bug is closed.
- **Could not run:** leave it open and say why in a comment only when that
  adds something new (a missing plugin, an unclear step).

## 6. Shrink the queue

A test you ran here without eyes, ears or a device-specific quirk could
probably run on the server. When you see a concrete way (recorded MIDI
bytes, the offline engine, a loopback), say so in your closing comment, and
open one issue labelled `testing` for it. Do not open one per bench issue.

## 7. Report

End with a table: issue, result, evidence in one line. Then what is still
open and which device or person each one waits for, and whether a release is
now free (`python3 scripts/ci/bench.py gate`).
