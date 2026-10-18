import { describe, expect, test } from 'bun:test'
import { PublicKey } from '@solana/web3.js'
import { mintTransferFee, mintTransferHookProgram } from '../src/core/decoders'

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
