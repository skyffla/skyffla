# Publishing

The Node.js wrapper lives at `wrappers/node` and publishes to npm as `skyffla`.

## Versioning

- Release versions:
  - the npm package version lives in `wrappers/node/package.json`
  - the runtime wrapper version also lives in `wrappers/node/src/version.js`
  - both should track the repo release tag version exactly
  - a Git tag `vX.Y.Z` should only be pushed after the Node.js package version
    is set to `X.Y.Z`

- Protocol compatibility is a separate contract. See:
  - `docs/versioning.md`
  - `docs/machine-protocol.md`

The wrapper validates machine protocol compatibility from `room_welcome` and
requires the same machine protocol major version as the published package.

Official packages also probe `skyffla --version` before starting a room and
reject a CLI binary whose release version does not exactly match the npm
package version. This is a wrapper packaging policy on top of machine protocol
compatibility. `SKYFFLA_SKIP_VERSION_CHECK=1` bypasses only the release-version
pairing check.

## Release Flow

1. Update the release version in:
   - `Cargo.toml`
   - `wrappers/node/package.json`
   - `wrappers/node/src/version.js`
2. Run the wrapper checks locally:

```sh
cargo test -p skyffla --test machine_local_end_to_end
npm test --prefix wrappers/node
```

3. Push the release tag:

```sh
git tag vX.Y.Z
git push origin vX.Y.Z
```

4. GitHub Actions will:
   - build the local `skyffla` binary needed by the smoke tests
   - run the Node.js wrapper test suite
   - pack the npm tarball
   - verify `package.json` matches the tag version
   - publish `skyffla` to npm only if the repository variable
     `PUBLISH_NODE_PACKAGE=1`

By default, tagged releases do not publish the npm package. This keeps normal
release tags safe until the npm package is claimed and trusted publishing is
configured.

## npm Setup

The `Node Wrapper` workflow uses npm trusted publishing.

Configure trusted publishing on npm for the GitHub repository that owns
`.github/workflows/node-package.yml`:

- workflow: `Node Wrapper`
- workflow file: `.github/workflows/node-package.yml`
- environment: not required

No npm automation token should be stored once trusted publishing is configured.

## Enable First Publish

When the `skyffla` npm name is ready:

1. Create or claim the `skyffla` package on npm.
2. Configure trusted publishing for the `Node Wrapper` workflow.
3. Set the repository variable `PUBLISH_NODE_PACKAGE=1`.
4. Cut or re-push the intended `vX.Y.Z` release tag.
