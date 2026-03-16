export const __version__: string;

export interface ProtocolVersion {
  major: number;
  minor: number;
}

export interface BlobRef {
  hash: string;
  format: BlobFormatValue;
}

export interface Member {
  member_id: string;
  name: string;
  fingerprint?: string;
}

export interface RouteAll {
  type: "all";
}

export interface RouteMember {
  type: "member";
  member_id: string;
}

export type Route = RouteAll | RouteMember;

export interface SendChat {
  type: "send_chat";
  to: Route;
  text: string;
}

export interface SendFile {
  type: "send_file";
  channel_id: string;
  to: Route;
  path: string;
  name?: string;
  mime?: string;
}

export interface OpenChannel {
  type: "open_channel";
  channel_id: string;
  kind: ChannelKindValue;
  to: Route;
  name?: string;
  size?: number;
  mime?: string;
  blob?: BlobRef;
}

export interface AcceptChannel {
  type: "accept_channel";
  channel_id: string;
}

export interface RejectChannel {
  type: "reject_channel";
  channel_id: string;
  reason?: string;
}

export interface SendChannelData {
  type: "send_channel_data";
  channel_id: string;
  body: string;
}

export interface CloseChannel {
  type: "close_channel";
  channel_id: string;
  reason?: string;
}

export interface ExportChannelFile {
  type: "export_channel_file";
  channel_id: string;
  path: string;
}

export type MachineCommand =
  | SendChat
  | SendFile
  | OpenChannel
  | AcceptChannel
  | RejectChannel
  | SendChannelData
  | CloseChannel
  | ExportChannelFile;

export interface RoomWelcome {
  type: "room_welcome";
  protocol_version: ProtocolVersion;
  room_id: string;
  self_member: string;
  host_member: string;
}

export interface MemberSnapshot {
  type: "member_snapshot";
  members: readonly Member[];
}

export interface MemberJoined {
  type: "member_joined";
  member: Member;
}

export interface MemberLeft {
  type: "member_left";
  member_id: string;
  reason?: string;
}

export interface Chat {
  type: "chat";
  from: string;
  from_name: string;
  to: Route;
  text: string;
}

export interface ChannelOpened {
  type: "channel_opened";
  channel_id: string;
  kind: ChannelKindValue;
  from: string;
  from_name: string;
  to: Route;
  name?: string;
  size?: number;
  mime?: string;
  blob?: BlobRef;
}

export interface ChannelAccepted {
  type: "channel_accepted";
  channel_id: string;
  member_id: string;
  member_name: string;
}

export interface ChannelRejected {
  type: "channel_rejected";
  channel_id: string;
  member_id: string;
  member_name: string;
  reason?: string;
}

export interface ChannelData {
  type: "channel_data";
  channel_id: string;
  from: string;
  from_name: string;
  body: string;
}

export interface ChannelClosed {
  type: "channel_closed";
  channel_id: string;
  member_id: string;
  member_name: string;
  reason?: string;
}

export interface ChannelFileReady {
  type: "channel_file_ready";
  channel_id: string;
  blob: BlobRef;
}

export interface ChannelFileExported {
  type: "channel_file_exported";
  channel_id: string;
  path: string;
  size: number;
}

export interface TransferProgress {
  type: "transfer_progress";
  channel_id: string;
  item_kind: TransferItemKindValue;
  name: string;
  phase: TransferPhaseValue;
  bytes_complete: number;
  bytes_total?: number;
}

export interface ErrorEvent {
  type: "error";
  code: string;
  message: string;
  channel_id?: string;
}

export type MachineEvent =
  | RoomWelcome
  | MemberSnapshot
  | MemberJoined
  | MemberLeft
  | Chat
  | ChannelOpened
  | ChannelAccepted
  | ChannelRejected
  | ChannelData
  | ChannelClosed
  | ChannelFileReady
  | ChannelFileExported
  | TransferProgress
  | ErrorEvent;

export type ChannelKindValue = "machine" | "file" | "clipboard";
export type BlobFormatValue = "blob" | "collection";
export type TransferItemKindValue = "file" | "folder";
export type TransferPhaseValue = "preparing" | "downloading" | "exporting";

export const ChannelKind: Readonly<{
  MACHINE: "machine";
  FILE: "file";
  CLIPBOARD: "clipboard";
}>;

export const BlobFormat: Readonly<{
  BLOB: "blob";
  COLLECTION: "collection";
}>;

export const TransferItemKind: Readonly<{
  FILE: "file";
  FOLDER: "folder";
}>;

export const TransferPhase: Readonly<{
  PREPARING: "preparing";
  DOWNLOADING: "downloading";
  EXPORTING: "exporting";
}>;

export const MACHINE_PROTOCOL_VERSION: Readonly<ProtocolVersion>;

export class SkyfflaError extends Error {}
export class SkyfflaProtocolError extends SkyfflaError {}
export class SkyfflaMachineProtocolMismatch extends SkyfflaProtocolError {}
export class SkyfflaProcessExited extends SkyfflaError {}
export class SkyfflaVersionMismatch extends SkyfflaError {}

export interface SpawnOptions {
  binary?: string;
  server?: string;
  name?: string;
  downloadDir?: string;
  local?: boolean;
}

export interface OpenChannelOptions {
  kind: ChannelKindValue;
  to: string | Route;
  name?: string;
  size?: number;
  mime?: string;
  blob?: BlobRef;
}

export interface OpenMachineChannelOptions {
  to: string | Route;
  name?: string;
}

export class MachineChannel {
  readonly room: Room;
  readonly channelId: string;
  send(body: string): Promise<void>;
  accept(): Promise<void>;
  reject(reason?: string): Promise<void>;
  close(reason?: string): Promise<void>;
  exportFile(path: string): Promise<void>;
}

export class Room implements AsyncIterable<MachineEvent> {
  readonly role: string;
  readonly roomId: string;
  readonly argv: readonly string[];
  static host(roomId: string, options?: SpawnOptions): Promise<Room>;
  static join(roomId: string, options?: SpawnOptions): Promise<Room>;
  channel(channelId: string): MachineChannel;
  send(command: MachineCommand): Promise<void>;
  sendChat(to: string | Route, text: string): Promise<void>;
  sendFile(
    channelId: string,
    to: string | Route,
    path: string,
    options?: { name?: string; mime?: string },
  ): Promise<void>;
  openChannel(channelId: string, options: OpenChannelOptions): Promise<MachineChannel>;
  openMachineChannel(channelId: string, options: OpenMachineChannelOptions): Promise<MachineChannel>;
  acceptChannel(channelId: string): Promise<void>;
  rejectChannel(channelId: string, options?: { reason?: string }): Promise<void>;
  sendChannelData(channelId: string, body: string): Promise<void>;
  closeChannel(channelId: string, options?: { reason?: string }): Promise<void>;
  exportChannelFile(channelId: string, path: string): Promise<void>;
  recv(): Promise<MachineEvent>;
  recvUntil(
    predicate: (event: MachineEvent) => boolean,
    options?: { timeout?: number },
  ): Promise<MachineEvent>;
  waitForWelcome(options?: { timeout?: number }): Promise<RoomWelcome>;
  waitForMemberSnapshot(options?: { minMembers?: number; timeout?: number }): Promise<MemberSnapshot>;
  waitForMemberJoined(options?: { name?: string; timeout?: number }): Promise<MemberJoined>;
  waitForChat(options?: { text?: string; fromName?: string; timeout?: number }): Promise<Chat>;
  waitForChannelOpened(options?: { channelId?: string; timeout?: number }): Promise<ChannelOpened>;
  waitForChannelData(options?: { channelId?: string; timeout?: number }): Promise<ChannelData>;
  stderrLines(): readonly string[];
  wait(): Promise<number>;
  close(): Promise<void>;
  [Symbol.asyncIterator](): AsyncIterator<MachineEvent>;
}

export function routeAll(): RouteAll;
export function routeMember(memberId: string): RouteMember;
export function normalizeRoute(route: string | Route): Route;
export function parseRoute(value: Route): Route;
export function parseMachineCommand(value: string | MachineCommand): MachineCommand;
export function parseMachineEvent(value: string | MachineEvent): MachineEvent;
export function dumpMessage(message: MachineCommand | MachineEvent): Record<string, unknown>;
export function dumpMessageJson(message: MachineCommand | MachineEvent): string;
export function parseCliVersion(text: string): string;
export function isProtocolCompatible(version: ProtocolVersion, other: ProtocolVersion): boolean;
export function ensureMachineProtocolVersion(version: ProtocolVersion): void;
export function ensureReleaseVersion(binaryVersion: string): void;
export function probeBinaryVersion(binary?: string): Promise<string>;
export function ensureBinaryVersion(binary?: string): Promise<void>;
