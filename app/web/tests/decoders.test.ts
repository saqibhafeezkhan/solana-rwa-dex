import { describe, expect, test } from 'bun:test'
import { PublicKey } from '@solana/web3.js'
import {
  ORDER_BOOK_LEN,
  ORDER_LEN,
  decodeOrder,
  decodeOrderBook,
  mintTransferFee,
  mintTransferHookProgram,
} from '../src/core/decoders'

/**
 * Golden mints, packed by the token program's own writer rather than by hand:
 * `StateWithExtensionsMut::<Mint>::unpack_uninitialized` + `init_extension` from
 * `spl-token-2022-interface`, the same code the token program runs, on a buffer
 * of `ExtensionType::try_calculate_account_len` bytes. Regenerate with a small
 * Rust program that prints `hex(data)` for each case below.
 *
 * The layout they pin down is the one an extension reader has to get right: the
 * 82-byte base state is padded out to the token-account length, the account-type
 * byte sits at 165, and the TLV entries start at 166 — not straight after the
 * mint's own 82 bytes.
 *
 * * `FEE_ONLY_MINT` — one `TransferFeeConfig` entry: an older fee of 50 bps
 *   (cap 1 000) from epoch 3 and a newer fee of 125 bps (cap 2 000) from epoch 7.
 * * `HOOK_MINT` — a `TransferHook` entry for a program of 32 `0x07` bytes, then a
 *   `TransferFeeConfig` entry of 25 bps (cap 500), so a reader that only looks at
 *   the first entry finds neither.
 */
const FEE_ONLY_MINT =
  '000000000000000000000000000000000000000000000000000000000000000000000000000000000000000006010000' +
  '000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000' +
  '000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000' +
  '0000000000000000000000000000000000000000000101006c0000000000000000000000000000000000000000000000' +
  '000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000' +
  '00000300000000000000e80300000000000032000700000000000000d0070000000000007d00'

const HOOK_MINT =
  '000000000000000000000000000000000000000000000000000000000000000000000000000000000000000006010000' +
  '000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000' +
  '000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000' +
  '000000000000000000000000000000000000000000010e00400000000000000000000000000000000000000000000000' +
  '00000000000000000000070707070707070707070707070707070707070707070707070707070707070701006c000909' +
  '090909090909090909090909090909090909090909090909090909090909000000000000000000000000000000000000' +
  '000000000000000000000000000000000000000000000100000000000000f40100000000000019000200000000000000' +
  'f4010000000000001900'

const bytes = (hex: string) => Uint8Array.from(Buffer.from(hex, 'hex'))

describe('mintTransferFee', () => {
  test('reports the fee in force for the epoch, older before the newer fee starts', () => {
    const mint = bytes(FEE_ONLY_MINT)
    expect(mintTransferFee(mint, 2n)).toEqual({ basisPoints: 50, maximumFee: 1000n })
    expect(mintTransferFee(mint, 6n)).toEqual({ basisPoints: 50, maximumFee: 1000n })
    expect(mintTransferFee(mint, 7n)).toEqual({ basisPoints: 125, maximumFee: 2000n })
    expect(mintTransferFee(mint, 99n)).toEqual({ basisPoints: 125, maximumFee: 2000n })
  })

  test('reads an entry that follows another extension', () => {
    expect(mintTransferFee(bytes(HOOK_MINT), 2n)).toEqual({ basisPoints: 25, maximumFee: 500n })
  })

  test('reads a mint with no TLV area, or one cut short, as having no fee', () => {
    expect(mintTransferFee(new Uint8Array(82), 9n)).toBeNull()
    expect(mintTransferFee(bytes(FEE_ONLY_MINT).subarray(0, 200), 9n)).toBeNull()
  })
})

describe('order book decoders', () => {
  const writeKey = (data: Uint8Array, offset: number, fill: number) => {
    data.set(new Uint8Array(32).fill(fill), offset)
  }

  test('decodes an Order field for field', () => {
    const data = new Uint8Array(ORDER_LEN)
    data.set([1, 2, 3, 4, 5, 6, 7, 8], 0)
    const view = new DataView(data.buffer)
    let offset = 8
    writeKey(data, offset, 0x11) // book
    offset += 32
    writeKey(data, offset, 0x22) // owner
    offset += 32
    view.setBigUint64(offset, 42n, true) // order id
    offset += 8
    data[offset] = 1 // Side::Bid
    offset += 1
    view.setBigUint64(offset, 2_000_000_000n, true) // price
    offset += 8
    view.setBigUint64(offset, 500n, true) // original
    offset += 8
    view.setBigUint64(offset, 300n, true) // remaining
    offset += 8
    view.setBigUint64(offset, 200n, true) // filled base
    offset += 8
    view.setBigUint64(offset, 400n, true) // filled quote
    offset += 8
    view.setBigInt64(offset, 1_700_000_000n, true) // created
    offset += 8
    view.setBigInt64(offset, 1_800_000_000n, true) // expiry

    const order = decodeOrder({
      pubkey: new PublicKey(new Uint8Array(32).fill(9)),
      owner: new PublicKey(new Uint8Array(32).fill(8)),
      data,
      lamports: 1,
    })
    expect(order.side).toBe('bid')
    expect(order.orderId).toBe(42n)
    expect(order.price).toBe(2_000_000_000n)
    expect(order.remainingAmount).toBe(300n)
    expect(order.filledBase).toBe(200n)
    expect(order.expiryTimestamp).toBe(1_800_000_000n)
  })

  test('decodes a Book and its rails', () => {
    const data = new Uint8Array(ORDER_BOOK_LEN)
    data.set([1, 2, 3, 4, 5, 6, 7, 8], 0)
    const view = new DataView(data.buffer)
    let offset = 8
    writeKey(data, offset, 0x01) // base
    offset += 32
    writeKey(data, offset, 0x02) // quote
    offset += 32
    writeKey(data, offset, 0x03) // authority
    offset += 32
    writeKey(data, offset, 0x04) // kyc config
    offset += 32
    writeKey(data, offset, 0x05) // hooks authority
    offset += 32
    view.setBigUint64(offset, 100n, true) // settlement slots
    offset += 8
    data[offset] = 1 // paused
    offset += 1
    data[offset] = 255 // bump

    const book = decodeOrderBook({
      pubkey: new PublicKey(new Uint8Array(32).fill(9)),
      owner: new PublicKey(new Uint8Array(32).fill(8)),
      data,
      lamports: 1,
    })
    expect(book.paused).toBe(true)
    expect(book.settlementSlots).toBe(100n)
    expect(book.kycConfig.toBase58()).toBe(new PublicKey(new Uint8Array(32).fill(4)).toBase58())
  })
})

describe('mintTransferHookProgram', () => {
  test('finds the hook program even when another extension is written first', () => {
    const expected = new PublicKey(new Uint8Array(32).fill(7)).toBase58()
    expect(mintTransferHookProgram(bytes(HOOK_MINT))?.toBase58()).toBe(expected)
  })

  test('reads a mint without a hook as having none', () => {
    expect(mintTransferHookProgram(bytes(FEE_ONLY_MINT))).toBeNull()
    expect(mintTransferHookProgram(new Uint8Array(82))).toBeNull()
  })
})
