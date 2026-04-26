import {
  SkyfflaMachineProtocolMismatch,
  SkyfflaProtocolError,
  SkyfflaVersionMismatch,
} from "./errors.js";
import { __version__ } from "./version.js";

export const ChannelKind = Object.freeze({
  MACHINE: "machine",
  FILE: "file",
  CLIPBOARD: "clipboard",
  PIPE: "pipe",
});

export const BlobFormat = Object.freeze({
  BLOB: "blob",
  COLLECTION: "collection",
});

export const TransferItemKind = Object.freeze({
  FILE: "file",
  FOLDER: "folder",
});

export const TransferPhase = Object.freeze({
  PREPARING: "preparing",
  DOWNLOADING: "downloading",
  EXPORTING: "exporting",
});

export const MACHINE_PROTOCOL_VERSION = Object.freeze({
  major: 2,
  minor: 1,
});

const ROUTE_TYPES = new Set(["all", "member"]);
const CHANNEL_KINDS = new Set(Object.values(ChannelKind));
const BLOB_FORMATS = new Set(Object.values(BlobFormat));
const TRANSFER_ITEM_KINDS = new Set(Object.values(TransferItemKind));
const TRANSFER_PHASES = new Set(Object.values(TransferPhase));
const MACHINE_COMMAND_TYPES = new Set([
  "send_chat",
  "send_file",
  "open_channel",
  "accept_channel",
  "reject_channel",
  "send_channel_data",
  "close_channel",
  "export_channel_file",
]);
const VERSION_RE = /\b(\d+\.\d+\.\d+)\b/;

function requireObject(value, fieldName) {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    throw new SkyfflaProtocolError(`${fieldName} must be an object`);
  }
  return value;
}

function requireString(value, fieldName) {
  if (typeof value !== "string" || value.trim() === "") {
    throw new SkyfflaProtocolError(`${fieldName} must be a non-empty string`);
  }
  return value;
}

function requireBody(value, fieldName) {
  if (typeof value !== "string" || value === "") {
    throw new SkyfflaProtocolError(`${fieldName} must be a non-empty string`);
  }
  return value;
}

function optionalString(value, fieldName) {
  if (value === undefined || value === null) {
    return undefined;
  }
  if (typeof value !== "string") {
    throw new SkyfflaProtocolError(`${fieldName} must be a string`);
  }
  return value;
}

function optionalInteger(value, fieldName, { min = 0 } = {}) {
  if (value === undefined || value === null) {
    return undefined;
  }
  if (!Number.isInteger(value) || value < min) {
    throw new SkyfflaProtocolError(`${fieldName} must be an integer >= ${min}`);
  }
  return value;
}

function requireInteger(value, fieldName, { min = 0 } = {}) {
  if (!Number.isInteger(value) || value < min) {
    throw new SkyfflaProtocolError(`${fieldName} must be an integer >= ${min}`);
  }
  return value;
}

function requireEnum(value, fieldName, allowed) {
  if (typeof value !== "string" || !allowed.has(value)) {
    throw new SkyfflaProtocolError(
      `${fieldName} must be one of: ${Array.from(allowed).join(", ")}`,
    );
  }
  return value;
}

function copyDefined(entries) {
  return Object.fromEntries(entries.filter(([, value]) => value !== undefined));
}

function parseBlobRef(value, fieldName = "blob") {
  const blob = requireObject(value, fieldName);
  return Object.freeze({
    hash: requireString(blob.hash, `${fieldName}.hash`),
    format: requireEnum(blob.format, `${fieldName}.format`, BLOB_FORMATS),
  });
}

export function routeAll() {
  return Object.freeze({ type: "all" });
}

export function routeMember(memberId) {
  return Object.freeze({
    type: "member",
    member_id: requireString(memberId, "member_id"),
  });
}

export function normalizeRoute(route) {
  if (route === "all") {
    return routeAll();
  }
  if (typeof route === "string") {
    return routeMember(route);
  }
  return parseRoute(route);
}

export function parseRoute(value) {
  const route = requireObject(value, "route");
  const type = requireEnum(route.type, "route.type", ROUTE_TYPES);
  if (type === "all") {
    return routeAll();
  }
  return routeMember(route.member_id);
}

function parseProtocolVersion(value, fieldName = "protocol_version") {
  const version = requireObject(value, fieldName);
  return Object.freeze({
    major: requireInteger(version.major, `${fieldName}.major`, { min: 1 }),
    minor: requireInteger(version.minor, `${fieldName}.minor`, { min: 0 }),
  });
}

export function isProtocolCompatible(version, other) {
  return version.major === other.major;
}

export function ensureMachineProtocolVersion(version) {
  const parsed = parseProtocolVersion(version);
  if (!isProtocolCompatible(parsed, MACHINE_PROTOCOL_VERSION)) {
    throw new SkyfflaMachineProtocolMismatch(
      `skyffla machine protocol ${parsed.major}.${parsed.minor} is incompatible with Node wrapper protocol ${MACHINE_PROTOCOL_VERSION.major}.${MACHINE_PROTOCOL_VERSION.minor}. Wrappers require the same machine protocol major version.`,
    );
  }
}

function parseMember(value, fieldName = "member") {
  const member = requireObject(value, fieldName);
  return Object.freeze(
    copyDefined([
      ["member_id", requireString(member.member_id, `${fieldName}.member_id`)],
      ["name", requireString(member.name, `${fieldName}.name`)],
      ["fingerprint", optionalString(member.fingerprint, `${fieldName}.fingerprint`)],
    ]),
  );
}

function parseOpenChannelLike(input, fieldName = "message") {
  const blob = input.blob == null ? undefined : parseBlobRef(input.blob);
  const kind = requireEnum(input.kind, `${fieldName}.kind`, CHANNEL_KINDS);
  if (kind === ChannelKind.FILE && blob === undefined) {
    throw new SkyfflaProtocolError(`${fieldName}.blob is required for file channels`);
  }
  if (kind !== ChannelKind.FILE && blob !== undefined) {
    throw new SkyfflaProtocolError(`${fieldName}.blob is only allowed for file channels`);
  }
  return Object.freeze(
    copyDefined([
      ["channel_id", requireString(input.channel_id, `${fieldName}.channel_id`)],
      ["kind", kind],
      ["name", optionalString(input.name, `${fieldName}.name`)],
      ["size", optionalInteger(input.size, `${fieldName}.size`, { min: 0 })],
      ["mime", optionalString(input.mime, `${fieldName}.mime`)],
      ["blob", blob],
    ]),
  );
}

export function parseMachineCommand(value) {
  const payload = typeof value === "string" ? JSON.parse(value) : value;
  const message = requireObject(payload, "command");
  const type = requireString(message.type, "command.type");

  switch (type) {
    case "send_chat":
      return Object.freeze({
        type,
        to: normalizeRoute(message.to),
        text: requireString(message.text, "command.text"),
      });
    case "send_file":
      return Object.freeze(
        copyDefined([
          ["type", type],
          ["channel_id", requireString(message.channel_id, "command.channel_id")],
          ["to", normalizeRoute(message.to)],
          ["path", requireString(message.path, "command.path")],
          ["name", optionalString(message.name, "command.name")],
          ["mime", optionalString(message.mime, "command.mime")],
        ]),
      );
    case "open_channel": {
      const parsed = parseOpenChannelLike(message, "command");
      return Object.freeze({
        type,
        ...parsed,
        to: normalizeRoute(message.to),
      });
    }
    case "accept_channel":
      return Object.freeze({
        type,
        channel_id: requireString(message.channel_id, "command.channel_id"),
      });
    case "reject_channel":
      return Object.freeze(
        copyDefined([
          ["type", type],
          ["channel_id", requireString(message.channel_id, "command.channel_id")],
          ["reason", optionalString(message.reason, "command.reason")],
        ]),
      );
    case "send_channel_data":
      return Object.freeze({
        type,
        channel_id: requireString(message.channel_id, "command.channel_id"),
        body: requireBody(message.body, "command.body"),
      });
    case "close_channel":
      return Object.freeze(
        copyDefined([
          ["type", type],
          ["channel_id", requireString(message.channel_id, "command.channel_id")],
          ["reason", optionalString(message.reason, "command.reason")],
        ]),
      );
    case "export_channel_file":
      return Object.freeze({
        type,
        channel_id: requireString(message.channel_id, "command.channel_id"),
        path: requireString(message.path, "command.path"),
      });
    default:
      throw new SkyfflaProtocolError(`unsupported machine command type: ${type}`);
  }
}

export function parseMachineEvent(value) {
  const payload = typeof value === "string" ? JSON.parse(value) : value;
  const message = requireObject(payload, "event");
  const type = requireString(message.type, "event.type");

  switch (type) {
    case "room_welcome":
      return Object.freeze({
        type,
        protocol_version: parseProtocolVersion(message.protocol_version),
        room_id: requireString(message.room_id, "event.room_id"),
        self_member: requireString(message.self_member, "event.self_member"),
        host_member: requireString(message.host_member, "event.host_member"),
      });
    case "member_snapshot": {
      if (!Array.isArray(message.members) || message.members.length === 0) {
        throw new SkyfflaProtocolError("event.members must be a non-empty array");
      }
      return Object.freeze({
        type,
        members: Object.freeze(message.members.map((member) => parseMember(member))),
      });
    }
    case "member_joined":
      return Object.freeze({ type, member: parseMember(message.member) });
    case "member_left":
      return Object.freeze(
        copyDefined([
          ["type", type],
          ["member_id", requireString(message.member_id, "event.member_id")],
          ["reason", optionalString(message.reason, "event.reason")],
        ]),
      );
    case "chat":
      return Object.freeze({
        type,
        from: requireString(message.from, "event.from"),
        from_name: requireString(message.from_name, "event.from_name"),
        to: parseRoute(message.to),
        text: requireString(message.text, "event.text"),
      });
    case "channel_opened": {
      const parsed = parseOpenChannelLike(message, "event");
      return Object.freeze({
        type,
        ...parsed,
        from: requireString(message.from, "event.from"),
        from_name: requireString(message.from_name, "event.from_name"),
        to: parseRoute(message.to),
      });
    }
    case "channel_accepted":
      return Object.freeze({
        type,
        channel_id: requireString(message.channel_id, "event.channel_id"),
        member_id: requireString(message.member_id, "event.member_id"),
        member_name: requireString(message.member_name, "event.member_name"),
      });
    case "channel_rejected":
      return Object.freeze(
        copyDefined([
          ["type", type],
          ["channel_id", requireString(message.channel_id, "event.channel_id")],
          ["member_id", requireString(message.member_id, "event.member_id")],
          ["member_name", requireString(message.member_name, "event.member_name")],
          ["reason", optionalString(message.reason, "event.reason")],
        ]),
      );
    case "channel_data":
      return Object.freeze({
        type,
        channel_id: requireString(message.channel_id, "event.channel_id"),
        from: requireString(message.from, "event.from"),
        from_name: requireString(message.from_name, "event.from_name"),
        body: requireBody(message.body, "event.body"),
      });
    case "channel_closed":
      return Object.freeze(
        copyDefined([
          ["type", type],
          ["channel_id", requireString(message.channel_id, "event.channel_id")],
          ["member_id", requireString(message.member_id, "event.member_id")],
          ["member_name", requireString(message.member_name, "event.member_name")],
          ["reason", optionalString(message.reason, "event.reason")],
        ]),
      );
    case "channel_file_ready":
      return Object.freeze({
        type,
        channel_id: requireString(message.channel_id, "event.channel_id"),
        blob: parseBlobRef(message.blob),
      });
    case "channel_file_exported":
      return Object.freeze({
        type,
        channel_id: requireString(message.channel_id, "event.channel_id"),
        path: requireString(message.path, "event.path"),
        size: requireInteger(message.size, "event.size", { min: 0 }),
      });
    case "transfer_progress":
      return Object.freeze(
        copyDefined([
          ["type", type],
          ["channel_id", requireString(message.channel_id, "event.channel_id")],
          ["item_kind", requireEnum(message.item_kind, "event.item_kind", TRANSFER_ITEM_KINDS)],
          ["name", requireString(message.name, "event.name")],
          ["phase", requireEnum(message.phase, "event.phase", TRANSFER_PHASES)],
          ["bytes_complete", requireInteger(message.bytes_complete, "event.bytes_complete", { min: 0 })],
          ["bytes_total", optionalInteger(message.bytes_total, "event.bytes_total", { min: 0 })],
        ]),
      );
    case "error":
      return Object.freeze(
        copyDefined([
          ["type", type],
          ["code", requireString(message.code, "event.code")],
          ["message", requireString(message.message, "event.message")],
          ["channel_id", optionalString(message.channel_id, "event.channel_id")],
        ]),
      );
    default:
      throw new SkyfflaProtocolError(`unsupported machine event type: ${type}`);
  }
}

export function dumpMessage(message) {
  const parsed = MACHINE_COMMAND_TYPES.has(message?.type)
    ? parseMachineCommand(message)
    : parseMachineEvent(message);
  return JSON.parse(JSON.stringify(parsed));
}

export function dumpMessageJson(message) {
  return JSON.stringify(dumpMessage(message));
}

export function parseCliVersion(text) {
  const match = VERSION_RE.exec(text);
  if (match === null) {
    throw new SkyfflaVersionMismatch(
      `could not parse skyffla version from output: ${JSON.stringify(text)}`,
    );
  }
  return match[1];
}

export function shouldSkipVersionCheck() {
  const value = process.env.SKYFFLA_SKIP_VERSION_CHECK?.trim().toLowerCase() ?? "";
  return value === "1" || value === "true" || value === "yes" || value === "on";
}

export function ensureReleaseVersion(binaryVersion) {
  if (binaryVersion !== __version__) {
    throw new SkyfflaVersionMismatch(
      `skyffla Node package ${__version__} requires matching skyffla binary, got ${binaryVersion}. Install the corresponding CLI version or set SKYFFLA_SKIP_VERSION_CHECK=1 to bypass the check.`,
    );
  }
}
