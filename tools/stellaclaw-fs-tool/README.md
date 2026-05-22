# stellaclaw-fs-tool

Standalone filesystem tool used by Stellaclaw local and remote tool runtimes.

It exposes small filesystem subcommands. Most commands write stable JSON to
stdout; `file-bytes` writes raw file bytes to stdout for binary staging. Local
and remote Stellaclaw tool runtimes use the same CLI contract but cache the
platform-specific binary where the command actually executes.

```bash
stellaclaw-fs-tool apply-patch --workspace /path/to/workspace --format auto < patch.txt
stellaclaw-fs-tool file-read --workspace /path/to/workspace --file-path src/main.rs --start-line 1 --end-line 80
stellaclaw-fs-tool file-write --workspace /path/to/workspace --file-path notes.txt --mode overwrite < notes.txt
stellaclaw-fs-tool file-hash --workspace /path/to/workspace --file-path image.png
stellaclaw-fs-tool file-bytes --workspace /path/to/workspace --file-path image.png > image.png
```

`file-hash` prints file size and SHA-256 as JSON. `file-bytes` lets callers
stage binary files without depending on remote Python, shell-specific `cat`
behavior, or text encoding.

Codex patch format is implemented without external dependencies. Unified diff
format delegates to `git apply`.

`grep` and `rg` are intentionally not bundled here. They should be provided as
official standalone search tools; shell usage can ensure `rg` independently.

Linux release assets use musl targets and keep the existing `linux-x64` /
`linux-arm64` platform names, so the binaries are statically linked and do not
depend on the remote host glibc version.

This package has its own version in `tools/stellaclaw-fs-tool/VERSION`.
