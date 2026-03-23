# Skyffla Node.js Examples

These examples are intentionally separate from the wrapper source tree, but in a
repository checkout they install the local `wrappers/node` package through a
relative file dependency. That keeps the examples aligned with the current
checkout instead of whichever wrapper release happens to be published on npm.

## Setup

Install `skyffla` first, for example with Homebrew:

```sh
brew install skyffla
```

Then change into this directory:

```sh
cd examples/node
npm install
```

If you want to try the published wrapper instead, replace the local file
dependency in `package.json` with the published npm package version.

If you want to run against a local repo build instead of the installed binary,
set `SKYFFLA_BIN`, for example `SKYFFLA_BIN=../../target/debug/skyffla`.

## Included Examples

- [`simple-chat`](simple-chat): minimal interactive chat client for one room
- [`sync-chat-and-channel`](sync-chat-and-channel): chat plus one machine channel exchange

## Run

Simple chat:

```sh
npm run simple-chat -- join demo-room --local
npm run simple-chat -- join demo-room --local
```

Chat plus machine channel:

```sh
npm run sync-chat-and-channel -- host demo-room
npm run sync-chat-and-channel -- join demo-room
```
