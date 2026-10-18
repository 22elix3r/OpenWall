# Contributing

Start with the [README](README.md), [architecture](docs/architecture.md), and
[milestone gates](ROADMAP.md). Small, independently testable changes are welcome.

Use current stable Rust and the desktop dependencies listed in the README. Run formatting,
Clippy and the relevant tests before submitting a pull request. Changes to desktop or GPU
code should include OS/compositor/GPU versions and actual hardware evidence. A compile
check alone does not establish desktop support. Keep unsafe/native code small and explain
pointer, thread and lifetime requirements at each boundary.

Push changes to a feature branch and open a pull request. The protected `main` branch
requires current Ubuntu and macOS CI checks and resolved review conversations, including
for maintainers. Force pushes and branch deletion are disabled. An approving review is
not currently mandatory while the project has a single maintainer.

Compatibility work needs an original minimal fixture, a description of the observed
behavior/reference version, expected output and explicit unsupported cases. Do not upload
third-party wallpaper content or copy proprietary engine resources into fixtures. Keep
normal application code and data readers independent of Qt and platform backends.

By contributing, you agree to the Developer Certificate of Origin 1.1:
https://developercertificate.org/. Sign commits with `git commit -s`. No copyright
assignment or proprietary dual-license agreement is required. Original application code
uses AGPL-3.0-or-later; independently created sample content uses CC0-1.0.

Be respectful, give concrete evidence, and keep reviews focused on the work. Report
security issues through the process in [SECURITY.md](SECURITY.md).
