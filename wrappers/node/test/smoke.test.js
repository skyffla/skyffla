import test from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import { randomUUID } from "node:crypto";
import { access } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { Room, SkyfflaProcessExited } from "../src/index.js";

const RENDEZVOUS_VERSION_HEADER = "x-skyffla-rendezvous-version";
const RENDEZVOUS_API_VERSION = "1.0";

async function findBinary(t) {
  if (process.env.SKYFFLA_BIN) {
    return process.env.SKYFFLA_BIN;
  }

  const here = path.dirname(fileURLToPath(import.meta.url));
  const binary = path.resolve(here, "../../../target/debug/skyffla");
  try {
    await access(binary);
    return binary;
  } catch {
    t.skip("set SKYFFLA_BIN or build target/debug/skyffla to run smoke tests");
    return null;
  }
}

function isPermissionError(error) {
  const message = String(error?.message ?? error);
  return (
    message.includes("Operation not permitted") ||
    message.includes("permission denied") ||
    message.includes("listen EPERM")
  );
}

class RendezvousStub {
  constructor() {
    this.rooms = new Map();
    this.server = http.createServer(this.handle.bind(this));
  }

  get url() {
    const address = this.server.address();
    return `http://127.0.0.1:${address.port}`;
  }

  async start() {
    await new Promise((resolve, reject) => {
      this.server.listen(0, "127.0.0.1", resolve);
      this.server.once("error", reject);
    });
  }

  async stop() {
    await new Promise((resolve, reject) => {
      this.server.close((error) => (error ? reject(error) : resolve()));
    });
  }

  async waitForRoom(roomId, timeout = 5_000) {
    const deadline = Date.now() + timeout;
    while (Date.now() < deadline) {
      if (this.rooms.has(roomId)) {
        return;
      }
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
    throw new Error(`room ${roomId} was not registered`);
  }

  handle(req, res) {
    if (req.method === "GET" && req.url === "/health") {
      this.sendJson(res, 200, { status: "ok" });
      return;
    }

    const roomId = this.roomId(req.url);
    if (!roomId) {
      res.statusCode = 404;
      res.end();
      return;
    }

    if (req.method === "GET") {
      const record = this.rooms.get(roomId);
      const now = Math.floor(Date.now() / 1000);
      if (!record || record.expires_at_epoch_seconds <= now) {
        this.rooms.delete(roomId);
        res.statusCode = 404;
        res.end();
        return;
      }
      this.sendJson(res, 200, {
        room_id: roomId,
        ticket: record.ticket,
        expires_at_epoch_seconds: record.expires_at_epoch_seconds,
        capabilities: record.capabilities,
      });
      return;
    }

    if (req.method === "PUT") {
      let body = "";
      req.setEncoding("utf8");
      req.on("data", (chunk) => {
        body += chunk;
      });
      req.on("end", () => {
        const payload = JSON.parse(body);
        const expiresAt = Math.floor(Date.now() / 1000) + Number(payload.ttl_seconds);
        const existing = this.rooms.get(roomId);
        if (existing && existing.ticket !== payload.ticket) {
          this.sendJson(res, 409, {
            error: "room_already_exists",
            message: `room ${roomId} is already hosted`,
          });
          return;
        }
        this.rooms.set(roomId, {
          ticket: payload.ticket,
          capabilities: payload.capabilities,
          expires_at_epoch_seconds: expiresAt,
        });
        this.sendJson(res, 200, {
          room_id: roomId,
          expires_at_epoch_seconds: expiresAt,
          status: "waiting",
        });
      });
      return;
    }

    if (req.method === "DELETE") {
      this.rooms.delete(roomId);
      res.statusCode = 204;
      res.setHeader(RENDEZVOUS_VERSION_HEADER, RENDEZVOUS_API_VERSION);
      res.end();
      return;
    }

    res.statusCode = 405;
    res.end();
  }

  roomId(url) {
    const parts = url.split("/").filter(Boolean);
    if (parts.length === 3 && parts[0] === "v1" && parts[1] === "rooms") {
      return parts[2];
    }
    return null;
  }

  sendJson(res, status, payload) {
    const body = JSON.stringify(payload);
    res.statusCode = status;
    res.setHeader("content-type", "application/json");
    res.setHeader("content-length", Buffer.byteLength(body));
    res.setHeader(RENDEZVOUS_VERSION_HEADER, RENDEZVOUS_API_VERSION);
    res.end(body);
  }
}

test("wrapper can join chat and open machine channel", async (t) => {
  const binary = await findBinary(t);
  if (binary === null) {
    return;
  }
  const server = new RendezvousStub();
  try {
    await server.start();
  } catch (error) {
    if (isPermissionError(error)) {
      t.skip("local environment does not allow Skyffla transport sockets");
      return;
    }
    throw error;
  }

  const roomId = `node-smoke-${randomUUID().slice(0, 8)}`;
  const host = await Room.host(roomId, { binary, server: server.url, name: "host" });
  let join;

  try {
    try {
      const welcome = await host.waitForWelcome();
      assert.equal(welcome.type, "room_welcome");
    } catch (error) {
      if (isPermissionError(error)) {
        t.skip("local environment does not allow Skyffla transport sockets");
        return;
      }
      throw error;
    }

    await server.waitForRoom(roomId);
    join = await Room.join(roomId, { binary, server: server.url, name: "join" });

    try {
      await join.waitForMemberSnapshot({ minMembers: 2 });
      const joined = await host.waitForMemberJoined({ name: "join" });
      const joinMemberId = joined.member.member_id;

      await host.sendChat("all", "hello from node");
      const chat = await join.waitForChat({
        text: "hello from node",
        fromName: "host",
      });
      assert.equal(chat.from_name, "host");

      const channel = await host.openMachineChannel("c1", {
        to: joinMemberId,
        name: "agent-link",
      });
      const opened = await join.waitForChannelOpened({ channelId: "c1" });
      assert.equal(opened.type, "channel_opened");
      await join.channel("c1").send("sketch line");
      const data = await host.waitForChannelData({ channelId: channel.channelId, timeout: 60_000 });
      assert.equal(data.from_name, "join");
      assert.equal(data.body, "sketch line");
    } catch (error) {
      if (isPermissionError(error)) {
        t.skip("local environment does not allow Skyffla transport sockets");
        return;
      }
      throw error;
    }
  } finally {
    if (join) {
      await join.close();
    }
    await host.close();
    await server.stop();
  }
});
