import type { PublicKey } from '@solana/web3.js'
import { TOKEN_PROGRAM_ID } from '../core/pdas'
import { tokenProgramForMint } from '../core/connection'
import { useAccountState } from './useAccountState'
import { useApp } from './useApp'

/**
 * The token program a pool's mint is issued under, defaulting to legacy SPL
 * Token until the mint account is read. Pool clients must derive ATAs and pass
 * the token-program account from this value: a Token-2022 mint's ATAs live under
 * a different program id, so the legacy default silently addresses accounts
 * that do not exist.
 */
export function useTokenProgram(mint: PublicKey | null | undefined): PublicKey {
  const { connection } = useApp()
  const resolved = useAccountState(
    async () => (mint ? tokenProgramForMint(connection, mint) : TOKEN_PROGRAM_ID),
    [connection, mint?.toBase58()],
  )
  return resolved.state ?? TOKEN_PROGRAM_ID
}
