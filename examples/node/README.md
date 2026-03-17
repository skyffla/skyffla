# Skyffla Node.js Examples

These examples are intentionally separate from the wrapper source tree. They
install the published [`skyffla`](https://www.npmjs.com/package/skyffla)
package from npm so the setup stays close to what an external user would do.
The checked-in dependency version should track the latest published wrapper release.

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
