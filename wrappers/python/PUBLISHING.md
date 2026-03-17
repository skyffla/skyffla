# Publishing

The Python wrapper lives at `wrappers/python` and publishes to PyPI as
`skyffla`.

## Versioning

- Release versions:
  - the Python package version lives in `wrappers/python/pyproject.toml`
  - the runtime wrapper version also lives in `wrappers/python/src/skyffla/__about__.py`
  - the runnable example dependency also lives in `examples/python/pyproject.toml`
  - both wrapper files and the example dependency should track the repo release tag version exactly
  - a Git tag `vX.Y.Z` should only be pushed after the Python package version is
    set to `X.Y.Z`

- Protocol compatibility is a separate contract. See:
  - `docs/versioning.md`
  - `docs/machine-protocol.md`

The wrapper validates machine protocol compatibility from `room_welcome` and
requires the same machine protocol major version as the published package.

Official packages also probe `skyffla --version` before starting a room and
reject a CLI binary whose release version does not exactly match the Python
package version. This is a wrapper packaging policy on top of machine protocol
compatibility. `SKYFFLA_SKIP_VERSION_CHECK=1` bypasses only the release-version
pairing check.

## Release Flow

1. Update the release version in:
   - `Cargo.toml`
   - `wrappers/python/pyproject.toml`
   - `wrappers/python/src/skyffla/__about__.py`
   - `examples/python/pyproject.toml`
   - `examples/python/uv.lock`
2. Run the wrapper checks locally:

```sh
cargo test -p skyffla --test machine_local_end_to_end
uv run --project wrappers/python pytest wrappers/python/tests -q
```

3. Push the release tag:

```sh
git tag vX.Y.Z
git push origin vX.Y.Z
```

4. GitHub Actions will:
   - build the local `skyffla` binary needed by the smoke tests
   - run the Python wrapper test suite
   - build the sdist and wheel
   - verify `pyproject.toml` matches the tag version
   - publish `skyffla` to PyPI only if the repository variable `PUBLISH_PYTHON_PACKAGE=1`

The runnable examples are treated as release consumers, not separate packages.
They should depend on the same published wrapper version as the release they ship with.

By default, tagged releases do not publish the Python package. This keeps normal
release tags safe until the PyPI package is claimed and trusted publishing is
configured.

## PyPI Setup

The `Python Wrapper` workflow uses trusted publishing.

Configure a trusted publisher on PyPI for the GitHub repository that owns
`.github/workflows/python-package.yml`:

- workflow: `Python Wrapper`
- workflow file: `.github/workflows/python-package.yml`
- environment: not required

No API token should be stored once trusted publishing is configured.

## Enable First Publish

When the `skyffla` PyPI name is ready:

1. Create or claim the `skyffla` project on PyPI.
2. Configure trusted publishing for the `Python Wrapper` workflow.
3. Set the repository variable `PUBLISH_PYTHON_PACKAGE=1`.
4. Cut or re-push the intended `vX.Y.Z` release tag.
