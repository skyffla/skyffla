# Skyffla Node.js Wrapper

Use this package to control an installed `skyffla` CLI from Node.js.

`npm install skyffla` installs the Node.js wrapper only. Install the `skyffla`
binary separately first.

## 1. Install the `skyffla` binary

Add the Homebrew tap once:

```sh
brew tap skyffla/skyffla
```

Install the CLI:

```sh
brew install skyffla
```

Check that the binary is available:

```sh
skyffla --version
```

If you want source code, release notes, or other install paths, see the
[Skyffla repository on GitHub](https://github.com/skyffla/skyffla).

## 2. Install the Node.js wrapper

```sh
npm install skyffla
```

The wrapper expects a `skyffla` binary in `PATH`. Override it with
`SKYFFLA_BIN` or the `binary` option when starting a room.

## 3. Small example

Save this as `example.mjs`. Start `host` in one terminal, then start `join` in
another.

```js
import { Room } from "skyffla";

async function main(role) {
  const opener = role === "host" ? Room.host : Room.join;
  const room = await opener("warehouse", { name: role });

  try {
    await room.waitForWelcome();
    if (role === "host") {
      const joined = await room.waitForMemberJoined({ name: "join" });
      await room.sendChat("all", "hello from node");
      console.log(await room.waitForChat({ fromName: "join" }));
      await room.openMachineChannel("node-demo", {
        to: joined.member.member_id,
        name: "node-demo",
      });
    } else {
      await room.waitForMemberSnapshot({ minMembers: 2 });
      console.log(await room.waitForChat({ fromName: "host" }));
      await room.sendChat("all", "hello from join");
      const opened = await room.waitForChannelOpened({ channelId: "node-demo" });
      await room.channel(opened.channel_id).send("hello over machine channel");
    }
  } finally {
    await room.close();
  }
}

await main(process.argv[2]);
```

```sh
node example.mjs host
node example.mjs join
```

## Version checks

Versioning is split across two layers:

- machine protocol compatibility follows the
  [versioning policy](https://github.com/skyffla/skyffla/blob/main/docs/versioning.md)
  and
  [machine protocol reference](https://github.com/skyffla/skyffla/blob/main/docs/machine-protocol.md):
  the wrapper requires the same machine protocol major version and tolerates
  additive minor changes
- release pairing is stricter for published packages: by default, the wrapper
  also checks that the spawned `skyffla` binary has the exact same release
  version as the Node.js package

Set `SKYFFLA_SKIP_VERSION_CHECK=1` only if you intentionally want to run the
wrapper against a different `skyffla` release. This skips the package-to-binary
release match, but machine protocol compatibility is still enforced.

## More examples

Runnable example apps live under `examples/node` in the GitHub repository:

- [examples/node](https://github.com/skyffla/skyffla/tree/main/examples/node):
  shared Node.js example project
- [examples/node/simple-chat](https://github.com/skyffla/skyffla/tree/main/examples/node/simple-chat):
  minimal interactive chat client for one room
- [examples/node/sync-chat-and-channel](https://github.com/skyffla/skyffla/tree/main/examples/node/sync-chat-and-channel):
  chat plus one machine-channel exchange

## Tests

From a repository checkout:

```sh
cd wrappers/node
npm test
```
