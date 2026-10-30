/**
 * Token-2022 `TokenMetadata` extension reader.
 *
 * The extension stores, after the update authority and mint, three borsh
 * strings in order: name, symbol, uri. The base mint and the extension TLV
 * headers shift that prefix around, so we scan for the first plausible
 * (name, symbol) pair and then read the trailing uri — the same tolerant
 * approach the portfolio scanner uses for the symbol alone.
 */
import type { Connection, PublicKey } from '@solana/web3.js'

export interface TokenMetadataView {
  name: string | null
  symbol: string | null
  /** Off-chain document pointer (e.g. the NAV attestation PDF). */
  uri: string | null
}

const u32le = (data: Uint8Array, at: number) =>
  ((data[at] ?? 0) | ((data[at + 1] ?? 0) << 8) | ((data[at + 2] ?? 0) << 16) | ((data[at + 3] ?? 0) << 24)) >>> 0

function readString(data: Uint8Array, at: number, max: number): { value: string; next: number } | null {
  if (at + 4 > data.length) return null
  const length = u32le(data, at)
  if (length < 1 || length > max) return null
  if (at + 4 + length > data.length) return null
  let value = ''
  for (let i = 0; i < length; i++) {
    const byte = data[at + 4 + i] ?? 0
    if (byte < 0x20 || byte > 0x7e) return null
    value += String.fromCharCode(byte)
  }
  return { value, next: at + 4 + length }
}

export function parseTokenMetadata(data: Uint8Array): TokenMetadataView {
  for (let p = 82; p + 8 <= data.length; p++) {
    const name = readString(data, p, 32)
    if (!name) continue
    const symbol = readString(data, name.next, 10)
    if (!symbol) continue
    const uri = readString(data, symbol.next, 512)
    return { name: name.value, symbol: symbol.value, uri: uri ? uri.value : null }
  }
  return { name: null, symbol: null, uri: null }
}

export async function loadTokenMetadata(connection: Connection, mint: PublicKey): Promise<TokenMetadataView> {
  const info = await connection.getAccountInfo(mint)
  if (!info) throw new Error(`mint ${mint.toBase58()} not found`)
  return parseTokenMetadata(info.data)
}

/**
 * Best-effort detection of an ISIN / CUSIP in the metadata text. For a North
 * American ISIN the CUSIP is the ISIN without its two-letter prefix and check
 * digit, so we derive it rather than matching it separately (a CUSIP embedded
 * in an ISIN has no word boundary around it).
 */
export function detectIdentifiers(text: string): { isin: string | null; cusip: string | null } {
  const upper = text.toUpperCase()
  const isinMatch = upper.match(/\b[A-Z]{2}[A-Z0-9]{9}[0-9]\b/)
  if (isinMatch) return { isin: isinMatch[0], cusip: isinMatch[0].slice(2, 11) }
  const cusipMatch = upper.match(/\b[A-Z0-9]{9}\b/)
  return { isin: null, cusip: cusipMatch ? cusipMatch[0] : null }
}
