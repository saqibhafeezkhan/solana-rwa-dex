/**
 * Minimal Anchor-compatible borsh encoding, enough to serialize the arguments
 * of every instruction this app calls. Instruction data is always:
 *
 *   [8-byte anchor discriminator][borsh-serialized args]
 *
 * where the discriminator is the first 8 bytes of sha256(`global:<snake_case_name>`).
 * Strings are borsh `String` (u32 LE length + utf8 bytes) and fixed arrays are
 * written element by element.
 */

const DISCRIMINATOR_CACHE = new Map<string, Uint8Array>()

/** FNV-1a is NOT sha256, but is only used to pre-size the buffer below. */
function roughSize(parts: unknown[]): number {
  let size = 8
  for (const part of parts) {
    if (typeof part === 'number') size += 16
    else if (typeof part === 'boolean') size += 1
    else if (typeof part === 'string') size += 4 + part.length
    else if (part instanceof Uint8Array) size += 4 + part.length
    else if (Array.isArray(part)) size += roughSize(part)
    else if (part && typeof part === 'object') {
      const values = Object.values(part as Record<string, unknown>)
      size += roughSize(values)
    }
  }
  return size
}

export function snakeCase(name: string): string {
  return name.replace(/([a-z0-9])([A-Z])/g, '$1_$2').toLowerCase()
}

/** sha256 via the Web Crypto API, available in every modern browser. */
async function sha256(message: string): Promise<Uint8Array> {
  const digest = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(message))
  return new Uint8Array(digest)
}

/** Anchor's sighash for `global:<snake_case_name>`, cached per instruction. */
export async function discriminator(instructionName: string): Promise<Uint8Array> {
  const key = snakeCase(instructionName)
  const cached = DISCRIMINATOR_CACHE.get(key)
  if (cached) return cached
  const digest = await sha256(`global:${key}`)
  const first8 = digest.slice(0, 8)
  DISCRIMINATOR_CACHE.set(key, first8)
  return first8
}

/**
 * A growable byte writer with borsh primitives. Numbers accept `number` for
 * convenience (exact while < 2^53) and `bigint` for full u64/u128 range.
 */
export class BorshWriter {
  private buffer: Uint8Array
  private length = 0

  constructor(preSizedBy?: unknown[]) {
    this.buffer = new Uint8Array(Math.max(64, roughSize(preSizedBy ?? []) * 2))
  }

  private ensure(extra: number) {
    if (this.length + extra <= this.buffer.length) return
    let capacity = this.buffer.length
    while (capacity < this.length + extra) capacity *= 2
    const next = new Uint8Array(capacity)
    next.set(this.buffer.subarray(0, this.length))
    this.buffer = next
  }

  u8(value: number) {
    this.ensure(1)
    this.buffer[this.length] = value & 0xff
    this.length += 1
  }

  bool(value: boolean) {
    this.u8(value ? 1 : 0)
  }

  private fixed(width: number, value: bigint) {
    this.ensure(width)
    let v = value
    for (let i = 0; i < width; i++) {
      this.buffer[this.length + i] = Number(v & 0xffn)
      v >>= 8n
    }
    this.length += width
  }

  u16(value: number | bigint) {
    this.fixed(2, BigInt(value))
  }

  u32(value: number | bigint) {
    this.fixed(4, BigInt(value))
  }

  u64(value: number | bigint) {
    this.fixed(8, BigInt(value))
  }

  u128(value: number | bigint) {
    this.fixed(16, BigInt(value))
  }

  i32(value: number) {
    this.fixed(4, BigInt.asUintN(32, BigInt(value)))
  }

  i64(value: number | bigint) {
    this.fixed(8, BigInt.asUintN(64, BigInt(value)))
  }

  string(value: string) {
    const bytes = new TextEncoder().encode(value)
    this.u32(bytes.length)
    this.ensure(bytes.length)
    this.buffer.set(bytes, this.length)
    this.length += bytes.length
  }

  /** A plain byte payload, written as-is (no length prefix). */
  raw(bytes: Uint8Array) {
    this.ensure(bytes.length)
    this.buffer.set(bytes, this.length)
    this.length += bytes.length
  }

  /** A borsh fixed-size array of 32-byte values, prefixed with its count. */
  pubkeyArray(keys: { toBytes(): Uint8Array }[]) {
    this.u32(keys.length)
    for (const key of keys) this.raw(key.toBytes())
  }

  /** A borsh `Vec<[u8; 32]>` — Merkle proofs. */
  proofArray(proofs: Uint8Array[]) {
    this.u32(proofs.length)
    for (const proof of proofs) {
      if (proof.length !== 32) throw new Error('Merkle proof elements must be 32 bytes')
      this.raw(proof)
    }
  }

  toUint8Array(): Uint8Array {
    return this.buffer.subarray(0, this.length)
  }
}

/**
 * Encodes one instruction call: discriminator + args. `write` receives the
 * writer and appends the borsh args in the instruction's declared order.
 */
export async function encodeInstruction(
  name: string,
  write?: (w: BorshWriter) => void,
): Promise<Uint8Array> {
  const writer = new BorshWriter()
  writer.raw(await discriminator(name))
  write?.(writer)
  return writer.toUint8Array()
}
