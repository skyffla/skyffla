import process from "node:process";

import { Room } from "skyffla";

async function main() {
  if (process.argv.length !== 4) {
    console.error("usage: sync-chat-and-channel <host|join> <room-id>");
    process.exitCode = 2;
    return;
  }

  const role = process.argv[2];
  const roomId = process.argv[3];
  const roomFactory = role === "host" ? Room.host : Room.join;
  const room = await roomFactory(roomId, { name: `node-${role}` });

  try {
    const welcome = await room.waitForWelcome();
    console.log(
      `joined room=${welcome.room_id} self=${welcome.self_member} host=${welcome.host_member}`,
    );

    const snapshot = await room.waitForMemberSnapshot();
    console.log("members:", snapshot.members.map((member) => member.name).join(", "));

    await room.sendChat("all", `hello from ${role}`);
    console.log("chat event:", await room.waitForChat());

    if (role === "host") {
      const joined = await room.waitForMemberJoined({ timeout: 60_000 });
      await room.openMachineChannel("node-demo", {
        to: joined.member.member_id,
        name: "node-demo",
      });
      console.log(
        "channel opened:",
        await room.waitForChannelData({ channelId: "node-demo", timeout: 60_000 }),
      );
    } else {
      const opened = await room.waitForChannelOpened({ channelId: "node-demo", timeout: 60_000 });
      await room.channel(opened.channel_id).send("hello over machine channel");
    }
  } finally {
    await room.close();
  }
}

main().catch((error) => {
  console.error(error.message);
  process.exitCode = 1;
});
