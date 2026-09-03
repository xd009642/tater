# Tater

## Description

Tarpaulin Crater (tater), dumb name project useful for probably just me. It'll
download all the repos in a list running tarpaulin on them with debug logging
enabled and save the stdout and debug logs in a directory.

## Repository inputs

`--input` accepts both Tater's original object-shaped manifest and the JSON array
written by the sibling `repos` collector. Collected entries retain their Tarpaulin
arguments and per-command `+toolchain`. CI-only upload credentials and arguments
whose values are unresolved CI expressions are omitted.

For collected entries, Tater inspects the referenced GitHub Actions workflow after
cloning. Invocations which only run on Windows or Apple runners, or which select an
embedded, WebAssembly, non-Linux, or different-architecture target, are not run.
Unrecognised and self-hosted configurations are treated as potentially compatible
rather than guessed at. Skipped project IDs and reasons are written to `output/skip`;
they do not make the run fail.

Each collected CI invocation is a separate project. This preserves distinct feature
and engine configurations from the same repository, and their result IDs include the
command metadata so concurrent invocations cannot share a checkout.

## Disk usage

Tater deletes each checkout after its run succeeds. Failed checkouts are also
deleted by default; pass `--retain-failed` to compress them into
`results/<project>/checkout.zip` instead. Build artifacts in `target` are never
included in retained archives.

Use `--disk-budget 5GB` (decimal units) or `--disk-budget 5GiB` (binary units)
to cap Tater's output. Usage is checked before and after each project and every
two seconds while Tarpaulin runs. If the cap is exceeded, Tater terminates the
current process group, cleans its checkout without retaining it, saves progress,
and exits with an error.

## Concurrency

Use `--project-jobs N` to run up to N repositories concurrently. This is
separate from `--jobs`, which limits Cargo jobs inside every repository, so the
maximum potential Cargo parallelism is their product. Project results may
finish out of order, but checkpoints and the `pass` and `fail` reports are
written by one coordinator in the original input order.

## License

Tater is licensed under MIT for what it's worth.
