import { createContext } from 'react'
import type { Connection, Keypair, PublicKey, TransactionInstruction } from '@solana/web3.js'
import type { ClusterName, WalletLike } from '../core/connection'
import type { Role } from './enterprise'

export interface Toast {
  id: number
  kind: 'pending' | 'confirmed' | 'error'
  text: string
  signature?: string
}

export type ExecutionMode = 'direct' | 'multisig'

/** A serialized instruction, so a proposal survives a page reload. */
export interface ProposalInstruction {
  programId: string
  keys: { pubkey: string; isSigner: boolean; isWritable: boolean }[]
  /** base64 instruction data */
  data: string
}

export interface Proposal {
  id: number
  label: string
  createdAt: number
  createdBy: string | null
  instructions: ProposalInstruction[]
  status: 'pending' | 'approved' | 'rejected'
}

/** One instruction to send, or a promise for one; nullish entries are dropped. */
export type SendableInstruction =
  | TransactionInstruction
  | null
  | undefined
  | PromiseLike<TransactionInstruction | null | undefined>

export interface AppContextValue {
  cluster: ClusterName
  setCluster: (cluster: ClusterName) => void
  connection: Connection
  wallet: WalletLike | null
  publicKey: PublicKey | null
  connecting: boolean
  connect: () => Promise<void>
  disconnect: () => Promise<void>
  /**
   * Sends + confirms one instruction bundle and reports via toasts. In
   * `multisig` execution mode it is queued as a proposal instead of broadcast.
   */
  send: (
    label: string,
    instructions: SendableInstruction[],
    extraSigners?: Keypair[],
  ) => Promise<void>
  toasts: Toast[]
  dismiss: (id: number) => void
  /** Role-based access control. */
  role: Role
  setRole: (role: Role) => void
  /** When 'multisig', `send` stages proposals instead of broadcasting. */
  executionMode: ExecutionMode
  setExecutionMode: (mode: ExecutionMode) => void
  /** Renders the demo dataset on every page and blocks transactions. */
  sampleMode: boolean
  setSampleMode: (enabled: boolean) => void
  proposals: Proposal[]
  approveProposal: (id: number) => Promise<void>
  rejectProposal: (id: number) => void
  clearProposals: () => void
}

export const AppContext = createContext<AppContextValue | null>(null)
