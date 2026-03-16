import process from "node:process";

import { Room } from "skyffla";

function parseArgs(argv) {
  const options = {
    server: undefined,
    local: false,
    name: undefined,
  };

  const positionals = [];
  for (let index = 0; index < argv.length; index += 1) {
    const value = argv[index];
    if (value === "--server") {
      index += 1;
      options.server = argv[index];
      continue;
    }
    if (value === "--local") {
      options.local = true;
      continue;
    }
    if (value === "--name") {
      index += 1;
      options.name = argv[index];
      continue;
    }
    positionals.push(value);
  }

  if (positionals.length !== 2) {
    throw new Error("usage: simple-chat <host|join> <room-id> [--server URL] [--local] [--name NAME]");
  }

  const [role, roomId] = positionals;
  if (role !== "host" && role !== "join") {
    throw new Error("role must be host or join");
  }

  return { role, roomId, ...options };
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const roomFactory = args.role === "host" ? Room.host : Room.join;
  const room = await roomFactory(args.roomId, {
    name: args.name ?? `node-${args.role}`,
    server: args.server,
    local: args.local,
  });

  try {
    const welcome = await room.waitForWelcome();
    console.log(
      `connected room=${welcome.room_id} self=${welcome.self_member} host=${welcome.host_member}`,
    );

    if (args.role === "join" && welcome.self_member === welcome.host_member) {
      console.log("[system] no existing host was found; this join promoted itself to host");
    }

    const snapshot = await room.waitForMemberSnapshot({ minMembers: 1 });
    console.log("members:", snapshot.members.map((member) => member.name).join(", "));
    if (snapshot.members.length === 1) {
      console.log("[system] waiting for another member to join");
    }
    console.log("type a message and press Enter. use /quit to exit.");

    const stop = new AbortController();

    const recvLoop = (async () => {
      while (!stop.signal.aborted) {
        try {
          const event = await room.recv();
          if (event.type === "chat") {
            console.log(`[${event.from_name}] ${event.text}`);
          } else if (event.type === "member_joined") {
            console.log(`[system] ${event.member.name} joined`);
          } else if (event.type === "member_left") {
            console.log(`[system] ${event.member_id} left`);
          } else if (event.type === "error") {
            console.log(`[error] ${event.code}: ${event.message}`);
          } else {
            console.log(`[event] ${JSON.stringify(event)}`);
          }
        } catch (error) {
          if (!stop.signal.aborted) {
            console.error(`[error] ${error.message}`);
          }
          stop.abort();
        }
      }
    })();

    process.stdin.setEncoding("utf8");
    for await (const line of process.stdin) {
      const text = line.replace(/\r?\n$/u, "");
      if (stop.signal.aborted) {
        break;
      }
      if (text === "/quit") {
        break;
      }
      if (text.trim() === "") {
        continue;
      }
      await room.sendChat("all", text);
    }

    stop.abort();
    await recvLoop.catch(() => {});
  } finally {
    await room.close();
  }
}

main().catch((error) => {
  console.error(error.message);
  process.exitCode = 1;
});
