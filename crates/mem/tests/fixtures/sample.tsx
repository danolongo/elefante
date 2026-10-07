/** Terminal client entry. Not the second sentence. */

import React, { useState } from "react";
import type { Frame } from './protocol';
export * from "./reducer";
export { send } from './client';

/** The API key. */
export const API_KEY = "sk-ant-api03-ABCDEFGH12345678";
const LIMIT = 10;

/** Renders one turn. Extra detail. */
export const Panel = (props: { n: number; token?: string }): JSX.Element => <div>{props.n}</div>;

export const withDefault = (key = "sk-ant-zzzzzzzzzzzzzz", n = 3) => key;

const helper = function inner(a: string): string { return a; };

/**
 * Connects to the daemon.
 * @param path the socket
 */
export async function connect<T extends object>(path: string, opts?: T): Promise<void> {
  const nested = () => 1;
}

export interface Transport {
  /** Sends a frame. */
  send(frame: Frame): void;
  readonly id: string;
}

export type Handler<T> = (value: T) => void;

export enum Mode { Raw, Cooked }

/** A session. */
export abstract class Session<T> extends Base implements Transport {
  private x = 1;
  /** Starts it. */
  async start(opts: Options = { key: "sk-ant-yyyyyyyyyyyyyy" }): Promise<void> {}
  static create(): Session<any> { return null as any; }
  abstract close(): void;
}

namespace Util {
  export function clamp(n: number): number { return n; }
}

declare function legacy(x: number): void;
