import { useState } from 'react'
import type { PublicKey } from '@solana/web3.js'

/**
 * Mint fields for a pool page. While sample mode is on the fields show the
 * demo pool's mints; switching it off restores whatever the user had typed.
 * Deriving the value during render keeps the toggle reactive without an effect.
 */
export function useSampleMints(
  sample: { mintA: PublicKey; mintB: PublicKey },
  sampleMode: boolean,
) {
  const [mintA, setMintA] = useState('')
  const [mintB, setMintB] = useState('')

  return {
    mintA: sampleMode ? sample.mintA.toBase58() : mintA,
    setMintA,
    mintB: sampleMode ? sample.mintB.toBase58() : mintB,
    setMintB,
  }
}
