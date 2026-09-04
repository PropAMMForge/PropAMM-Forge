/**
 * Reading events from transaction logs (FR-020).
 *
 * `emit!` in Anchor is `sol_log_data`, and in the logs it looks like the line
 * `Program data: <base64>`, where the first eight bytes are the event discriminator.
 *
 * # Why parsing the line is not enough
 *
 * `Program data:` **does not carry the address of the program that wrote it**. In
 * a transaction where our swap is invoked by a router next to another venue, the
 * other venue's events sit in the same array of lines. A match of eight bytes of
 * discriminator is no proof here: eight bytes are not a signature, and a foreign
 * event with the same prefix would parse into our `Swapped` with foreign numbers.
 *
 * So [`parseVaultEvents`] rebuilds the call stack from the lines
 * `Program <id> invoke [n]` / `success` / `failed` and takes only those `Program data:`
 * written while **our** program is on top of the stack. It is the same logic by
 * which the validator reads the log, and there is no other way to attribute an event from logs.
 *
 * # A limit to know about in advance
 *
 * An RPC provider may truncate transaction logs — and then the event simply does
 * not arrive, with no sign of an error. The known way out is `emit_cpi!`, which
 * writes the event into instruction data instead of the log; it costs an extra
 * account and noticeably more CU in `swap`, where the budget is tightest (SC-002),
 * so it is not chosen up front. If the collector (T042) runs into truncated logs,
 * that is a switch, not a new investigation.
 */

import { type Address, type ReadonlyUint8Array, getBase64Encoder } from '@solana/kit'
import {
  type CapitalMoved,
  type QuoteCleared,
  type QuoteUpdated,
  type Swapped,
  capitalMovedCodec,
  quoteClearedCodec,
  quoteUpdatedCodec,
  swappedCodec,
} from './codecs.js'
import { PROPAMM_VAULT_PROGRAM_ADDRESS, eventDiscriminator } from './program.js'

/** A parsed program event. */
export type VaultEvent =
  | { name: 'CapitalMoved'; data: CapitalMoved }
  | { name: 'QuoteCleared'; data: QuoteCleared }
  | { name: 'QuoteUpdated'; data: QuoteUpdated }
  | { name: 'Swapped'; data: Swapped }

/** An event together with its log line number — order within a transaction matters. */
export interface VaultEventAtLog {
  event: VaultEvent
  logIndex: number
}

const base64 = getBase64Encoder()

interface EventEntry {
  discriminator: Uint8Array
  decode(bytes: ReadonlyUint8Array, offset: number): VaultEvent
}

const EVENTS: readonly EventEntry[] = [
  {
    discriminator: eventDiscriminator('QuoteUpdated'),
    decode: (bytes, offset) => ({
      name: 'QuoteUpdated',
      data: quoteUpdatedCodec.decode(bytes, offset),
    }),
  },
  {
    discriminator: eventDiscriminator('QuoteCleared'),
    decode: (bytes, offset) => ({
      name: 'QuoteCleared',
      data: quoteClearedCodec.decode(bytes, offset),
    }),
  },
  {
    discriminator: eventDiscriminator('Swapped'),
    decode: (bytes, offset) => ({ name: 'Swapped', data: swappedCodec.decode(bytes, offset) }),
  },
  {
    discriminator: eventDiscriminator('CapitalMoved'),
    decode: (bytes, offset) => ({
      name: 'CapitalMoved',
      data: capitalMovedCodec.decode(bytes, offset),
    }),
  },
]

function matches(data: ReadonlyUint8Array, discriminator: Uint8Array): boolean {
  return data.length >= discriminator.length && discriminator.every((b, i) => data[i] === b)
}

/**
 * Parse the contents of one `Program data:`.
 *
 * Returns `null` if the discriminator is not ours — the logs also contain foreign
 * `Program data:`, and events this copy of the SDK does not know.
 */
export function decodeVaultEvent(data: ReadonlyUint8Array): VaultEvent | null {
  for (const entry of EVENTS) {
    if (matches(data, entry.discriminator)) {
      return entry.decode(data, entry.discriminator.length)
    }
  }
  return null
}

/** The same from a base64 string, as it sits in the log. */
export function decodeVaultEventFromBase64(encoded: string): VaultEvent | null {
  return decodeVaultEvent(base64.encode(encoded))
}

const INVOKE = /^Program (\S+) invoke \[\d+\]$/
const FINISH = /^Program (\S+) (?:success|failed)/
const DATA = /^Program data: (\S*)$/

/**
 * Pick the events of **our** program out of transaction logs, in order of appearance.
 *
 * `logs` is `meta.logMessages` from `getTransaction` or the field of a log subscription.
 */
export function parseVaultEvents(
  logs: readonly string[],
  programAddress: Address = PROPAMM_VAULT_PROGRAM_ADDRESS,
): VaultEventAtLog[] {
  const found: VaultEventAtLog[] = []
  const stack: string[] = []

  for (const [logIndex, line] of logs.entries()) {
    const invoke = INVOKE.exec(line)
    if (invoke?.[1] !== undefined) {
      stack.push(invoke[1])
      continue
    }
    if (FINISH.test(line)) {
      stack.pop()
      continue
    }
    const data = DATA.exec(line)
    if (data?.[1] === undefined || stack.at(-1) !== programAddress) {
      continue
    }
    const event = decodeVaultEventFromBase64(data[1])
    if (event !== null) {
      found.push({ event, logIndex })
    }
  }

  return found
}
