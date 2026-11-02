# yt-dlp-tg source repository

[README](README.md) and [SPEC](SPEC.md) define setup and behavior. Preserve
authorization, callback ownership, bounded subprocess/group cleanup, typed cache
identity and persisted queue ordering. Run one native compio/io_uring executor;
do not introduce a Tokio runtime.

Use jj feature changes and integrate completed work into dev. Read diffs with
git diff. The source tree must remain standalone: no private journals, operator
identifiers, workstation paths, credentials, runtime databases or references to
an enclosing coordination repository. Keep generic docs/tests self-contained.

Checks: cargo build --locked; cargo test --locked;
cargo clippy --locked --all-targets -- -D warnings; cargo fmt --check;
python3 tests/smoke.py after rebuilding. Python image/adapter checks use the
launching interpreter; install Pillow and the locked Node profile dependencies
when qualifying those cases. Mock workflows send no real Telegram messages.
Live sends and public publication require explicit operator instructions.

Never log tokens, proxy credentials, cookie contents, raw Telegram errors or
yt-dlp stderr. Preserve Apache-2.0 and third-party notices. Changes to behavior
must update SPEC first, with checks of affected consumers.
