### Added
- **Version and commit in the app:** Settings shows the version and the commit
  the build came from (`kabelsalat 0.1.0 (abc1234)`), and so does
  `kabelsalat --version`. Builds without a git checkout can pass the commit as
  `KABELSALAT_COMMIT`; otherwise it reads `unknown`.
