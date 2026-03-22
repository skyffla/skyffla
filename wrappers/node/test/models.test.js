import test from "node:test";
import assert from "node:assert/strict";

import {
  ChannelKind,
  MACHINE_PROTOCOL_VERSION,
  __version__,
  dumpMessage,
  ensureMachineProtocolVersion,
  normalizeRoute,
  parseCliVersion,
  parseMachineCommand,
  parseMachineEvent,
  routeAll,
} from "../src/index.js";
import { SkyfflaMachineProtocolMismatch } from "../src/errors.js";

test("documented chat event shape round trips", () => {
  const event = parseMachineEvent({
    type: "chat",
    from: "m2",
    from_name: "beta",
    to: { type: "all" },
    text: "hello",
  });

  assert.equal(event.type, "chat");
  assert.equal(event.from, "m2");
  assert.equal(event.from_name, "beta");
  assert.deepEqual(event.to, routeAll());
  assert.equal(event.text, "hello");
});

test("room welcome uses structured protocol version", () => {
  const event = parseMachineEvent({
    type: "room_welcome",
    protocol_version: { major: 2, minor: 0 },
    room_id: "warehouse",
    self_member: "m1",
    host_member: "m1",
  });

  assert.equal(event.type, "room_welcome");
  assert.deepEqual(event.protocol_version, MACHINE_PROTOCOL_VERSION);
  assert.deepEqual(dumpMessage(event), {
    type: "room_welcome",
    protocol_version: { major: 2, minor: 0 },
    room_id: "warehouse",
    self_member: "m1",
    host_member: "m1",
  });
});

test("documented open channel command shape round trips", () => {
  const command = parseMachineCommand({
    type: "open_channel",
    channel_id: "c7",
    kind: ChannelKind.MACHINE,
    to: { type: "member", member_id: "m2" },
    name: "agent-link",
  });

  assert.equal(command.type, "open_channel");
  assert.equal(command.channel_id, "c7");
  assert.equal(command.kind, ChannelKind.MACHINE);
  assert.deepEqual(command.to, { type: "member", member_id: "m2" });
  assert.deepEqual(dumpMessage(command), {
    type: "open_channel",
    channel_id: "c7",
    kind: "machine",
    to: { type: "member", member_id: "m2" },
    name: "agent-link",
  });
});

test("channel opened tolerates null blob for non-file channels", () => {
  const event = parseMachineEvent({
    type: "channel_opened",
    channel_id: "c7",
    kind: "machine",
    from: "m2",
    from_name: "beta",
    to: { type: "member", member_id: "m1" },
    name: "agent-link",
    blob: null,
  });

  assert.equal(event.type, "channel_opened");
  assert.equal(event.channel_id, "c7");
  assert.equal(event.kind, ChannelKind.MACHINE);
  assert.equal(event.from, "m2");
  assert.equal(event.from_name, "beta");
  assert.deepEqual(event.to, { type: "member", member_id: "m1" });
  assert.equal("blob" in event, false);
});

test("channel opened tolerates empty optional mime for non-file channels", () => {
  const event = parseMachineEvent({
    type: "channel_opened",
    channel_id: "c8",
    kind: "machine",
    from: "m2",
    from_name: "beta",
    to: { type: "member", member_id: "m1" },
    mime: "",
  });

  assert.equal(event.type, "channel_opened");
  assert.equal(event.mime, "");
});

test("normalize route accepts all and member ids", () => {
  assert.deepEqual(normalizeRoute("all"), { type: "all" });
  assert.deepEqual(normalizeRoute("m9"), { type: "member", member_id: "m9" });
});

test("parse cli version reads standard output", () => {
  assert.equal(parseCliVersion("skyffla 0.2.0"), "0.2.0");
  assert.equal(parseCliVersion(`skyffla ${__version__}`), __version__);
});

test("machine protocol version requires matching major", () => {
  ensureMachineProtocolVersion({ major: 2, minor: 7 });

  assert.throws(
    () => ensureMachineProtocolVersion({ major: 3, minor: 0 }),
    (error) =>
      error instanceof SkyfflaMachineProtocolMismatch &&
      error.message.includes("same machine protocol major version"),
  );
});

test("transfer progress event round trips", () => {
  const event = parseMachineEvent({
    type: "transfer_progress",
    channel_id: "img-bb3e8484",
    item_kind: "file",
    name: "Cyan-Tea-Confidential.png",
    phase: "preparing",
    bytes_complete: 65536,
    bytes_total: null,
  });

  assert.equal(event.type, "transfer_progress");
  assert.equal(event.channel_id, "img-bb3e8484");
  assert.equal(event.item_kind, "file");
  assert.equal(event.phase, "preparing");
  assert.equal(event.bytes_complete, 65536);
  assert.equal(event.bytes_total, undefined);
});
