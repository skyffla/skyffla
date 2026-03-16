# Simple Chat

Minimal interactive chat client using the Skyffla Node.js wrapper.

Install `skyffla`, then from this directory run:

```sh
npm install
```

Run two terminals:

```sh
npm run simple-chat -- join demo-room --local
npm run simple-chat -- join demo-room --local
```

If you want to use a local repo build of `skyffla` instead of the installed
binary, set `SKYFFLA_BIN=../../target/debug/skyffla`.
