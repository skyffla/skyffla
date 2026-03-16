export class SkyfflaError extends Error {
  constructor(message) {
    super(message);
    this.name = new.target.name;
  }
}

export class SkyfflaProtocolError extends SkyfflaError {}

export class SkyfflaMachineProtocolMismatch extends SkyfflaProtocolError {}

export class SkyfflaProcessExited extends SkyfflaError {}

export class SkyfflaVersionMismatch extends SkyfflaError {}
