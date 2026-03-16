# Sync Chat And Channel

Chat plus one machine-channel message exchange using the async Node.js wrapper.

Install `skyffla`, then from this directory run:

```sh
npm install
```

Run two terminals:

```sh
npm run sync-chat-and-channel -- host demo-room
npm run sync-chat-and-channel -- join demo-room
```

If you want to use a local repo build of `skyffla` instead of the installed
binary, set `SKYFFLA_BIN=../../target/debug/skyffla`.
