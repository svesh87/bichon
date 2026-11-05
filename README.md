# Mirror of rustmailer/bichon

This fork is a copy of [Bichon](https://github.com/rustmailer/bichon): `main` and the release
tags, synced daily by `.github/workflows/mirror.yml` on this branch (`mirror`, the default
branch, so the schedule runs). Fast-forward only, tags are never moved or deleted: if upstream
history is rewritten or removed, this copy keeps what it had.

The svesh changes to Bichon live in
[svesh87/bichon-patches](https://github.com/svesh87/bichon-patches), as patches on upstream
release tags; it falls back to this mirror when upstream is unreachable.
