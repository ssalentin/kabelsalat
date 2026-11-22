#!/usr/bin/env bash
# A PR labelled `enhancement` ships its README change in the same PR
# (AGENTS.md rule 132): the README is the shop window, and the plugin-editor
# windows (#71) merged without a line in "What you can do".
# Usage: readme-check.sh <labels> < changed-files
#   labels: comma-separated PR labels; changed files: one path per line
#   (real source: gh api "repos/<repo>/pulls/<n>/files" --jq '.[].filename').
# Exits 0 when the PR may merge, 1 when a feature PR is missing its README
# change. UI code without a screenshot change is only a warning: whether
# users can see the difference is the author's call.
# Self-test (real gh api output): scripts/ci/readme-check.sh --selftest
set -euo pipefail

ui_files='^src/(ui|theme)\.rs$'
shot_files='^docs/shots/.*\.(png|jpe?g|webp)$'

check() { # <labels> <changed-files>; prints annotations, returns the verdict
  local labels=",${1:-}," files="$2" shots ui msg
  if [[ $labels != *,enhancement,* ]]; then
    echo "not a feature PR (no 'enhancement' label) — ok"
    return 0
  fi
  shots=$(grep -E "$shot_files" <<<"$files" || true)
  ui=$(grep -E "$ui_files" <<<"$files" || true)
  if ! grep -qx 'README.md' <<<"$files"; then
    msg="labelled 'enhancement' (new feature) but README.md is not updated."
    msg+=" A new feature ships its README entry in the same PR: add or update"
    msg+=" its short line under \"What you can do\" (or \"Install\"), linking"
    msg+=" its section in docs/GUIDE.md, which holds the details."
    if [[ -n $ui && -z $shots ]]; then
      msg+=" This PR changes UI code, so also re-capture the screenshots that"
      msg+=" entry shows (scripts/shot.sh into docs/shots/, AGENTS.md rule 108)."
    fi
    msg+=" If this PR is not a new feature, remove the label."
    echo "::error::$msg"
    return 1
  fi
  if [[ -n $ui && -z $shots ]]; then
    echo "::warning file=${ui%%$'\n'*}::this feature changes UI code but no screenshot under docs/shots/ was updated. If users can see the difference, re-capture the README shots for this entry (scripts/shot.sh, AGENTS.md rule 108); if not, ignore this."
  fi
  echo "feature PR updates README.md — ok"
  return 0
}

selftest() {
  local pass=0 fail=0
  # Real `gh api repos/ssalentin/kabelsalat/pulls/<n>/files --jq
  # '.[].filename'` output, trimmed (rule 63): #71 is the plugin-editor
  # feature that merged WITHOUT a README change — the case this check
  # exists for; #81 is a CI-only PR.
  local pr71 pr81
  pr71=$(cat <<'EOF'
AGENTS.md
CHANGELOG.md
Cargo.lock
Cargo.toml
docs/ARCHITECTURE.md
packaging/flatpak/io.github.ssalentin.kabelsalat.yml
scripts/shot.sh
src/host.rs
src/main.rs
src/plugin_window.rs
src/ui.rs
EOF
)
  pr81=$(cat <<'EOF'
.github/workflows/ci.yml
scripts/ci/ubuntu-deps.sh
EOF
)
  local with_readme with_shots
  with_readme="$pr71"$'\n'README.md
  with_shots="$with_readme"$'\n'docs/shots/front.png

  expect() { # <name> <want-rc> <labels> <files> [<must-match>] [<must-NOT-match>]
    local name=$1 want=$2 out rc=0
    out=$(check "$3" "$4") || rc=$?
    if [[ $rc == "$want" ]] \
      && [[ -z ${5:-} || $out =~ $5 ]] \
      && { [[ -z ${6:-} ]] || [[ $out != *$6* ]]; }; then
      echo "  ok: $name"
      pass=$((pass + 1))
    else
      echo "  FAIL: $name (rc=$rc, want $want)"
      echo "  output: $out"
      fail=$((fail + 1))
    fi
  }

  expect "unlabelled PR passes (#81 real files)"        0 ""             "$pr81"        "not a feature PR"
  expect "feature without README fails (#71, the bug)"  1 "enhancement"  "$pr71"        "README.md is not updated"
  expect "label list still counts as feature"           1 "ui,enhancement" "$pr81"       "README.md is not updated"
  expect "README added, UI code, no shots -> warning"    0 "enhancement"  "$with_readme" "::warning file=src/ui.rs"
  expect "README and fresh shots -> no warning"          0 "enhancement"  "$with_shots"  ""            "::warning"
  expect "README without UI code -> no warning"         0 "enhancement"  "$pr81"$'\n'README.md "feature PR updates README.md" "::warning"
  expect "feature with only CI files still fails"       1 "enhancement"  "$pr81"        "README.md is not updated"

  echo "selftest: $pass ok, $fail failed"
  [[ $fail == 0 ]]
}

if [[ ${1:-} == --selftest ]]; then
  selftest
  exit
fi
check "${1:-}" "$(cat)"
