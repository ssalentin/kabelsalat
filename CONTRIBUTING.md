# Contributing to kabelsalat

Pull requests are welcome — bug fixes, plugins that misbehave, UI polish, docs,
new rack units. If you are new here, look for issues labelled
[`good first issue`](https://github.com/ssalentin/kabelsalat/labels/good%20first%20issue)
or [`help wanted`](https://github.com/ssalentin/kabelsalat/labels/help%20wanted).
For a bigger change, open an issue first so we can agree on the shape.

## Getting started

```sh
git clone https://github.com/ssalentin/kabelsalat.git
cd kabelsalat
cargo run                       # the app
cargo test --bin kabelsalat     # the unit suite
```

You need Rust (rustup), `alsa-lib`, `lilv` and PipeWire or ALSA. Surge XT
(CLAP) is the reference instrument; most self-tests use it.

## What a good pull request has

- **Evidence.** Every change is proven: tests for behaviour, and **screenshots**
  (before/after) for anything visible. The CLI self-tests
  (`kabelsalat record-test`, `rig-mix-test`, `cc-map-test`, …) cover the
  real audio paths — see [PLAN.md §0.1](PLAN.md).
- **A green suite.** `cargo test --bin kabelsalat` must pass.
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
`src/theme.rs` is the single palette source. No floating plugin windows —
editors are native egui panels.

## Releases

Versions follow [Semantic Versioning](https://semver.org/) and are tagged
`vX.Y.Z` on `main`; each tag gets a GitHub release with notes from
[CHANGELOG.md](CHANGELOG.md). Add your change under **Unreleased** there.

## License

By contributing you agree that your contribution is licensed under the
[MIT License](LICENSE).
