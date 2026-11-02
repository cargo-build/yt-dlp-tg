# Alpine/OpenRC deployment

[Project](../../README.md) · [Behavior](../../SPEC.md)

These are generic templates. Configure your own private allowlist, credential
files, host paths and dependency versions; do not put installed values in Git.
Build the x86_64-unknown-linux-musl executable on a suitable build host and
qualify it on the target before switching the single poller. Keep a complete
rollback release and a consistent redb backup after draining the old process.

The yt-dlp-tg OpenRC script expects an unprivileged yt-dlp-tg account and a
qualified release at /opt/yt-dlp-tg/current. Copy yt-dlp-tg.conf privately to
/etc/conf.d/yt-dlp-tg and set ALLOWED_USER_IDS there. The source template fails
without an explicit allowlist. Provision the token as an owner-only file;
protect state/log directories and preserve the existing cache during cutover.
Install FFmpeg, Deno and matching yt-dlp/EJS dependencies. Optional image input
also needs Pillow and the configured backend's pinned dependencies.

## Optional local Iggy service

Use the same source pin as Cargo.toml:
cargo-build/iggy5424fd3c800c29c3184e392394fbf6834e281b67. Build its server binary
for the target with --no-default-features --features mimalloc. The native client
requires authenticated loopback plain TCP; do not expose this service publicly.

The iggy-yt-dlp script uses a separate unprivileged account, executable under
/opt/iggy-yt-dlp/current, config /etc/iggy-yt-dlp/server.toml and private state
under /var/lib/iggy-yt-dlp. Copy iggy-yt-dlp.toml to that config path. Set0755
on every executable-directory parent; a private umask can otherwise make an
intermediate directory0700 even when the final release directory is0755.

Generate secure admin/app credentials privately using your secret manager.
For first boot only, place IGGY_ROOT_USERNAME/IGGY_ROOT_PASSWORD in an owner-only
server.env file selected by IGGY_ENV_PATH. Keep separate private admin/app
connection-string files; the bot account reads only the restricted app file.
After root metadata persists, remove the bootstrap variables and verify restart.

```sh
IGGY_CONNECTION_STRING_FILE=/private/admin-uri IGGY_APP_CONNECTION_STRING_FILE=/private/app-uri ./yt-dlp-tg iggy-provision
IGGY_CONNECTION_STRING_FILE=/private/app-uri ./yt-dlp-tg iggy-probe
```

Provisioning needs no Telegram token. It creates/verifies the owned stream,
one partition, persisted messages/offsets,16MiB segments, no preallocation,
no automatic expiry/size trimming and a restricted topic read/send/poll account.
Probe checks the native connection and topic profile. Protect both broker/redb
state; metadata is not the media file, and this is not a replicated deployment.

Configure the bot's private Iggy settings only after the broker is qualified.
OpenRC orders it after iggy-yt-dlp. Graceful stop drains its active delivery,
preserving unstarted work; retain the documented3600s grace for default limits.
Check actual child PID/user/executable, not supervisor status alone. Test clean
restarts and confirm one poller. Monitor resources and free disk; logs alone
are not an automatic alerting system.
