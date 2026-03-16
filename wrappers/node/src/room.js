import { spawn } from "node:child_process";
import { createInterface } from "node:readline";
import { setTimeout as delay } from "node:timers/promises";

import {
  ensureMachineProtocolVersion,
  ensureReleaseVersion,
  dumpMessageJson,
  normalizeRoute,
  parseCliVersion,
  parseMachineEvent,
  shouldSkipVersionCheck,
  ChannelKind,
} from "./protocol.js";
import {
  SkyfflaProcessExited,
  SkyfflaProtocolError,
  SkyfflaVersionMismatch,
} from "./errors.js";

function defaultBinary() {
  return process.env.SKYFFLA_BIN ?? "skyffla";
}

async function collectOutput(child) {
  const stdout = [];
  const stderr = [];

  if (child.stdout) {
    for await (const chunk of child.stdout) {
      stdout.push(chunk);
    }
  }
  if (child.stderr) {
    for await (const chunk of child.stderr) {
      stderr.push(chunk);
    }
  }

  return {
    stdout: Buffer.concat(stdout).toString("utf8").trim(),
    stderr: Buffer.concat(stderr).toString("utf8").trim(),
  };
}

export async function probeBinaryVersion(binary = defaultBinary()) {
  const child = spawn(binary, ["--version"], {
    stdio: ["ignore", "pipe", "pipe"],
  });
  const exitCode = await new Promise((resolve, reject) => {
    child.once("error", reject);
    child.once("exit", resolve);
  }).catch((error) => {
    throw new SkyfflaVersionMismatch(
      `failed to run ${JSON.stringify(binary)} --version: ${error.message}`,
    );
  });
  const output = await collectOutput(child);
  if (exitCode !== 0) {
    throw new SkyfflaVersionMismatch(
      `failed to run ${JSON.stringify(binary)} --version: ${output.stderr || output.stdout}`,
    );
  }
  return parseCliVersion(output.stdout);
}

export async function ensureBinaryVersion(binary = defaultBinary()) {
  if (shouldSkipVersionCheck()) {
    return;
  }
  ensureReleaseVersion(await probeBinaryVersion(binary));
}

class AsyncLineQueue {
  constructor(onClosed) {
    this._items = [];
    this._waiters = [];
    this._closedError = null;
    this._onClosed = onClosed;
  }

  push(value) {
    const waiter = this._waiters.shift();
    if (waiter) {
      waiter.resolve(value);
      return;
    }
    this._items.push(value);
  }

  close(error) {
    this._closedError = error;
    while (this._waiters.length > 0) {
      const waiter = this._waiters.shift();
      waiter.reject(error);
    }
    this._onClosed?.(error);
  }

  shift() {
    if (this._items.length > 0) {
      return Promise.resolve(this._items.shift());
    }
    if (this._closedError) {
      return Promise.reject(this._closedError);
    }
    return new Promise((resolve, reject) => {
      this._waiters.push({ resolve, reject });
    });
  }
}

export class MachineChannel {
  constructor(room, channelId) {
    this.room = room;
    this.channelId = channelId;
  }

  async send(body) {
    await this.room.sendChannelData(this.channelId, body);
  }

  async accept() {
    await this.room.acceptChannel(this.channelId);
  }

  async reject(reason) {
    await this.room.rejectChannel(this.channelId, { reason });
  }

  async close(reason) {
    await this.room.closeChannel(this.channelId, { reason });
  }

  async exportFile(path) {
    await this.room.exportChannelFile(this.channelId, path);
  }
}

export class Room {
  constructor({ role, roomId, argv, child }) {
    this.role = role;
    this.roomId = roomId;
    this.argv = Object.freeze([...argv]);
    this._child = child;
    this._stderrLines = [];
    this._readline = createInterface({
      input: child.stdout,
      crlfDelay: Infinity,
    });
    this._queue = new AsyncLineQueue();

    child.stderr.setEncoding("utf8");
    child.stderr.on("data", (chunk) => {
      for (const line of chunk.split(/\r?\n/u)) {
        if (line !== "") {
          this._stderrLines.push(line);
        }
      }
    });

    this._readline.on("line", (line) => {
      this._queue.push(line);
    });

    const closeWithExit = () => {
      this._queue.close(new SkyfflaProcessExited(this._exitMessage("skyffla closed stdout")));
    };

    this._readline.once("close", closeWithExit);
    child.once("error", (error) => {
      this._queue.close(
        new SkyfflaProcessExited(this._exitMessage(`failed starting skyffla: ${error.message}`)),
      );
    });
    child.once("exit", () => {
      if (!this._queue._closedError) {
        this._queue.close(new SkyfflaProcessExited(this._exitMessage("skyffla exited")));
      }
    });
  }

  static async host(roomId, options = {}) {
    return spawnRoom("host", roomId, options);
  }

  static async join(roomId, options = {}) {
    return spawnRoom("join", roomId, options);
  }

  channel(channelId) {
    return new MachineChannel(this, channelId);
  }

  async send(command) {
    if (this._child.stdin.destroyed) {
      throw new SkyfflaProcessExited(this._exitMessage("stdin is already closed"));
    }

    const payload = `${dumpMessageJson(command)}\n`;
    await new Promise((resolve, reject) => {
      this._child.stdin.write(payload, "utf8", (error) => {
        if (error) {
          reject(
            new SkyfflaProcessExited(
              this._exitMessage("failed writing command to skyffla"),
            ),
          );
          return;
        }
        resolve();
      });
    });
  }

  async sendChat(to, text) {
    await this.send({
      type: "send_chat",
      to: normalizeRoute(to),
      text,
    });
  }

  async sendFile(channelId, to, path, options = {}) {
    await this.send({
      type: "send_file",
      channel_id: channelId,
      to: normalizeRoute(to),
      path,
      name: options.name,
      mime: options.mime,
    });
  }

  async openChannel(channelId, options) {
    await this.send({
      type: "open_channel",
      channel_id: channelId,
      kind: options.kind,
      to: normalizeRoute(options.to),
      name: options.name,
      size: options.size,
      mime: options.mime,
      blob: options.blob,
    });
    return this.channel(channelId);
  }

  async openMachineChannel(channelId, options) {
    return this.openChannel(channelId, {
      kind: ChannelKind.MACHINE,
      ...options,
    });
  }

  async acceptChannel(channelId) {
    await this.send({
      type: "accept_channel",
      channel_id: channelId,
    });
  }

  async rejectChannel(channelId, options = {}) {
    await this.send({
      type: "reject_channel",
      channel_id: channelId,
      reason: options.reason,
    });
  }

  async sendChannelData(channelId, body) {
    await this.send({
      type: "send_channel_data",
      channel_id: channelId,
      body,
    });
  }

  async closeChannel(channelId, options = {}) {
    await this.send({
      type: "close_channel",
      channel_id: channelId,
      reason: options.reason,
    });
  }

  async exportChannelFile(channelId, path) {
    await this.send({
      type: "export_channel_file",
      channel_id: channelId,
      path,
    });
  }

  async recv() {
    while (true) {
      const line = await this._queue.shift();
      const text = line.trim();
      if (text === "") {
        continue;
      }

      let payload;
      try {
        payload = JSON.parse(text);
      } catch (error) {
        throw new SkyfflaProtocolError(`invalid machine event JSON: ${text}`);
      }

      const event = parseMachineEvent(payload);
      if (event.type === "room_welcome") {
        ensureMachineProtocolVersion(event.protocol_version);
      }
      return event;
    }
  }

  async recvUntil(predicate, { timeout = 30_000 } = {}) {
    const deadline = Date.now() + timeout;
    while (true) {
      const remaining = deadline - Date.now();
      if (remaining <= 0) {
        throw new Error(`timed out after ${timeout}ms waiting for machine event`);
      }
      const event = await Promise.race([
        this.recv(),
        delay(remaining).then(() => {
          throw new Error(`timed out after ${timeout}ms waiting for machine event`);
        }),
      ]);
      if (predicate(event)) {
        return event;
      }
    }
  }

  async waitForWelcome(options = {}) {
    return this.recvUntil((event) => event.type === "room_welcome", options);
  }

  async waitForMemberSnapshot({ minMembers = 1, timeout = 30_000 } = {}) {
    return this.recvUntil(
      (event) => event.type === "member_snapshot" && event.members.length >= minMembers,
      { timeout },
    );
  }

  async waitForMemberJoined({ name, timeout = 30_000 } = {}) {
    return this.recvUntil(
      (event) =>
        event.type === "member_joined" && (name === undefined || event.member.name === name),
      { timeout },
    );
  }

  async waitForChat({ text, fromName, timeout = 30_000 } = {}) {
    return this.recvUntil(
      (event) =>
        event.type === "chat" &&
        (text === undefined || event.text === text) &&
        (fromName === undefined || event.from_name === fromName),
      { timeout },
    );
  }

  async waitForChannelOpened({ channelId, timeout = 30_000 } = {}) {
    return this.recvUntil(
      (event) =>
        event.type === "channel_opened" &&
        (channelId === undefined || event.channel_id === channelId),
      { timeout },
    );
  }

  async waitForChannelData({ channelId, timeout = 30_000 } = {}) {
    return this.recvUntil(
      (event) =>
        event.type === "channel_data" &&
        (channelId === undefined || event.channel_id === channelId),
      { timeout },
    );
  }

  stderrLines() {
    return Object.freeze([...this._stderrLines]);
  }

  async wait() {
    if (this._child.exitCode !== null) {
      return this._child.exitCode;
    }
    return new Promise((resolve) => {
      this._child.once("exit", (code) => resolve(code ?? 0));
    });
  }

  async close() {
    if (!this._child.stdin.destroyed) {
      this._child.stdin.end();
    }

    if (this._child.exitCode === null) {
      this._child.kill("SIGTERM");
      const exited = await Promise.race([
        this.wait().then(() => true),
        delay(5_000).then(() => false),
      ]);
      if (!exited && this._child.exitCode === null) {
        this._child.kill("SIGKILL");
        await this.wait();
      }
    }

    this._readline.close();
  }

  [Symbol.asyncIterator]() {
    return {
      next: async () => {
        try {
          const value = await this.recv();
          return { value, done: false };
        } catch (error) {
          if (error instanceof SkyfflaProcessExited) {
            return { value: undefined, done: true };
          }
          throw error;
        }
      },
    };
  }

  _exitMessage(detail) {
    let message = `${detail}; argv=${JSON.stringify(this.argv)}`;
    if (this._child.exitCode !== null) {
      message += `; returncode=${this._child.exitCode}`;
    }
    if (this._stderrLines.length > 0) {
      message += `\nstderr:\n${this._stderrLines.slice(-20).join("\n")}`;
    }
    return message;
  }
}

async function spawnRoom(role, roomId, options) {
  const binary = options.binary ?? defaultBinary();
  await ensureBinaryVersion(binary);

  const argv = [role, roomId, "machine", "--json"];
  if (options.server !== undefined) {
    argv.push("--server", options.server);
  }
  if (options.name !== undefined) {
    argv.push("--name", options.name);
  }
  if (options.downloadDir !== undefined) {
    argv.push("--download-dir", options.downloadDir);
  }
  if (options.local) {
    argv.push("--local");
  }

  const child = spawn(binary, argv, {
    stdio: ["pipe", "pipe", "pipe"],
  });

  await new Promise((resolve, reject) => {
    child.once("spawn", resolve);
    child.once("error", (error) => {
      reject(
        new SkyfflaProcessExited(
          `failed starting skyffla process ${JSON.stringify(binary)}: ${error.message}`,
        ),
      );
    });
  });

  return new Room({
    role,
    roomId,
    argv: [binary, ...argv],
    child,
  });
}
