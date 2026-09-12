# Quality checks

The shared project baseline in [AGENTS.md](AGENTS.md) is authoritative.

Before a push, run the fast checks without rewriting source:

```sh
make lint static-analysis
```

The pull-request gate runs formatting, lint, static analysis, and Doxygen once;
then native Debian 13 amd64 and arm64 build, test, package, and staged-install
checks in parallel.  Production Rust code requires 100% line and branch
coverage on Debian 13 amd64 only.  The amd64 coverage job uses the pinned
nightly toolchain in `containers/quality.Dockerfile`, because Rust branch
coverage instrumentation is not yet stable.  It writes an ignored JSON report
under `build/coverage/`, excludes `tests/` and the in-tree `src/tests.rs` test
module, and fails if any remaining production `src/` line or branch is
uncovered.

Build and run that exact local quality image with:

```sh
make container-coverage
```

The target pulls the maintained Debian 13 `:latest` base before building the
derived image.  It then uses the normal launcher with an explicit local-image
exception, so the launcher does not try to pull the temporary local tag.

GitHub publishes the same derived quality image as one native amd64/arm64
manifest. Pull-request jobs use that published image; they do not use QEMU.

The repository owns no permanent local test container.  Use
`tools/run-in-quality-container.sh` so stale containers bearing this workspace's
three exact labels are cleaned before and after a run.  The launcher pulls a
remote image by default, prints the digest returned by that pull, and rejects a
run if it cannot identify the pulled digest.  `RPTADV_CONTAINER_PULL=0` is only
for a caller that just built a local derived image, such as
`make container-coverage`.
