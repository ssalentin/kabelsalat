# Contributing to kabelsalat

Pull requests are welcome — bug fixes, plugins that misbehave, UI polish, docs,
new rack units. If you are new here, look for issues labelled
[`good first issue`](https://github.com/ssalentin/kabelsalat/labels/good%20first%20issue)
or [`help wanted`](https://github.com/ssalentin/kabelsalat/labels/help%20wanted).
Comment on an issue before you start so nobody does the same work twice. For a
bigger change, open an issue first so we can agree on the shape. Planned work is
grouped into [milestones](https://github.com/ssalentin/kabelsalat/milestones);
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) explains how the app is built.

## Getting started

```sh
git clone https://github.com/ssalentin/kabelsalat.git
cd kabelsalat
cargo run                       # the app
cargo test --bin kabelsalat <filter>   # unit tests of what you changed
```

You need Rust (rustup), `alsa-lib`, `lilv` and PipeWire or ALSA. Surge XT
(CLAP) is the reference instrument; most self-tests use it.

## What a good pull request has

- **Evidence.** Every change is proven: tests for behaviour, and **screenshots**
  (before/after) for anything visible. The CLI self-tests
  (`kabelsalat record-test`, `rig-mix-test`, `cc-map-test`, …) cover the
  real audio paths — the full list is in [AGENTS.md](AGENTS.md#working-on-kabelsalat).
- **A green suite.** Run the unit tests of what you changed locally
  (`cargo test --bin kabelsalat <filter>`); CI runs the full suite on both
  architectures, and a PR merges only when it is green. Locally, spend the
  time on what CI cannot test: self-tests with real plugins, audio and MIDI
  hardware.
- **Its README entry.** A new feature updates [README.md](README.md) in the
  same PR — its entry under "What you can do" (or "Install"), plus fresh
  screenshots when it changes what users see (captured with
  [scripts/shot.sh](scripts/shot.sh), so nothing else on your screen is in
  them). PRs labelled
  [`enhancement`](https://github.com/ssalentin/kabelsalat/labels/enhancement)
  are checked by CI.
- **The project rules.** [AGENTS.md](AGENTS.md) lists rules learned from real
  bugs (realtime safety, agent parity, layout). They apply to humans too —
  in short: nothing allocates or locks on the audio thread, every UI action
  has an agent tool, and every fixed bug adds a rule.
- **Small and focused.** One topic per PR; commit messages follow
  [Conventional Commits](https://www.conventionalcommits.org/)
  (`feat(ui): …`, `fix(engine): …`, `docs: …`).

## Design

The visual language is described in
[docs/design-reference-analysis.md](docs/design-reference-analysis.md);
`src/theme.rs` is the single palette source. A plugin's own editor opens in
its own window on request — the main window never embeds one.

## Releases

Versions follow [Semantic Versioning](https://semver.org/) and are tagged
`vX.Y.Z` on `main`; each tag gets a GitHub release with notes from
[CHANGELOG.md](CHANGELOG.md). Add your change as a fragment under
[`changes/`](changes/README.md) instead of editing the changelog directly.

## License

By contributing you agree that your contribution is licensed under the
[MIT License](LICENSE).
