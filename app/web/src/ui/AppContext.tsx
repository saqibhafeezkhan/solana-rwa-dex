import { createContext, useCallback, useContext, useEffect, useMemo, useState } from 'react'
import type { ReactNode } from 'react'
import { Keypair, PublicKey, TransactionInstruction } from '@solana/web3.js'
import {
  connectionFor,
  describeError,
  getInjectedWallet,
  sendWithSigners,
  confirmTx,
  type ClusterName,
  type WalletLike,
} from '../core/connection'
import { isRole, type Role } from './enterprise'

interface Toast {
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

interface AppContextValue {
  cluster: ClusterName
  setCluster: (cluster: ClusterName) => void
  connection: ReturnType<typeof connectionFor>
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
    instructions: PromiseLike<TransactionInstruction>[],
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
  proposals: Proposal[]
  approveProposal: (id: number) => Promise<void>
  rejectProposal: (id: number) => void
  clearProposals: () => void
}

const AppContext = createContext<AppContextValue | null>(null)

const CLUSTER_KEY = 'rwadex.cluster'
const ROLE_KEY = 'rwadex.role'
const MODE_KEY = 'rwadex.executionMode'
const PROPOSALS_KEY = 'rwadex.proposals'

function bytesToBase64(bytes: Uint8Array): string {
  let binary = ''
  for (let i = 0; i < bytes.length; i++) binary += String.fromCharCode(bytes[i] ?? 0)
  return btoa(binary)
}

function base64ToBytes(value: string): Uint8Array {
  const binary = atob(value)
  const bytes = new Uint8Array(binary.length)
  for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i)
  return bytes
}

function toProposalInstruction(instruction: TransactionInstruction): ProposalInstruction {
  const data = instruction.data instanceof Uint8Array ? instruction.data : new Uint8Array(instruction.data)
  return {
    programId: instruction.programId.toBase58(),
    keys: instruction.keys.map((key) => ({
      pubkey: key.pubkey.toBase58(),
      isSigner: key.isSigner,
      isWritable: key.isWritable,
    })),
    data: bytesToBase64(data),
  }
}

function fromProposalInstruction(instruction: ProposalInstruction): TransactionInstruction {
  return new TransactionInstruction({
    programId: new PublicKey(instruction.programId),
    keys: instruction.keys.map((key) => ({
      pubkey: new PublicKey(key.pubkey),
      isSigner: key.isSigner,
      isWritable: key.isWritable,
    })),
    data: base64ToBytes(instruction.data),
  })
}

function loadProposals(): Proposal[] {
  try {
    const raw = localStorage.getItem(PROPOSALS_KEY)
    if (!raw) return []
    const parsed = JSON.parse(raw) as unknown
    return Array.isArray(parsed) ? (parsed as Proposal[]) : []
  } catch {
    return []
  }
}

function persistProposals(proposals: Proposal[]): Proposal[] {
  try {
    localStorage.setItem(PROPOSALS_KEY, JSON.stringify(proposals))
  } catch {
    // ignore storage failures
  }
  return proposals
}

export function AppProvider({ children }: { children: ReactNode }) {
  const [cluster, setClusterState] = useState<ClusterName>(() => {
    const stored = localStorage.getItem(CLUSTER_KEY)
    return stored === 'localnet' || stored === 'devnet' || stored === 'testnet' || stored === 'mainnet-beta'
      ? stored
      : 'localnet'
  })
  const [wallet, setWallet] = useState<WalletLike | null>(null)
  const [publicKey, setPublicKey] = useState<PublicKey | null>(null)
  const [connecting, setConnecting] = useState(false)
  const [toasts, setToasts] = useState<Toast[]>([])
  const connection = useMemo(() => connectionFor(cluster), [cluster])

  const [role, setRoleState] = useState<Role>(() => {
    const stored = localStorage.getItem(ROLE_KEY)
    return isRole(stored) ? stored : 'trader'
  })
  const setRole = useCallback((next: Role) => {
    localStorage.setItem(ROLE_KEY, next)
    setRoleState(next)
  }, [])

  const [executionMode, setExecutionModeState] = useState<ExecutionMode>(() =>
    localStorage.getItem(MODE_KEY) === 'multisig' ? 'multisig' : 'direct',
  )
  const setExecutionMode = useCallback((mode: ExecutionMode) => {
    localStorage.setItem(MODE_KEY, mode)
    setExecutionModeState(mode)
  }, [])

  const [proposals, setProposals] = useState<Proposal[]>(() => loadProposals())

  const setCluster = useCallback((next: ClusterName) => {
    localStorage.setItem(CLUSTER_KEY, next)
    setClusterState(next)
    setPublicKey(null)
    setWallet(null)
  }, [])

  const connect = useCallback(async () => {
    const provider = getInjectedWallet()
    if (!provider) {
      setToasts((t) => [
        ...t,
        { id: Date.now(), kind: 'error', text: 'No injected Solana wallet found. Install Phantom or Backpack.' },
      ])
      return
    }
    setConnecting(true)
    try {
      const response = await provider.connect()
      setPublicKey(response.publicKey)
      setWallet({
        publicKey: response.publicKey,
        signTransaction: (tx) => {
          const typed = provider.signTransaction
          if (!typed) throw new Error('wallet cannot sign transactions')
          return typed(tx as import('@solana/web3.js').Transaction) as never
        },
      })
    } catch (error) {
      setToasts((t) => [...t, { id: Date.now(), kind: 'error', text: describeError(error) }])
    } finally {
      setConnecting(false)
    }
  }, [])

  const disconnect = useCallback(async () => {
    const provider = getInjectedWallet()
    await provider?.disconnect().catch(() => undefined)
    setWallet(null)
    setPublicKey(null)
  }, [])

  const dismiss = useCallback((id: number) => {
    setToasts((t) => t.filter((toast) => toast.id !== id))
  }, [])

  /** Broadcasts and confirms one bundle; returns whether it landed. */
  const dispatch = useCallback(
    async (label: string, resolved: TransactionInstruction[], extraSigners?: Keypair[]): Promise<boolean> => {
      const toastId = Date.now() + Math.floor(Math.random() * 1000)
      try {
        setToasts((t) => [...t, { id: toastId, kind: 'pending', text: `${label}: sending…` }])
        const signature = await sendWithSigners(connection, wallet!, resolved, extraSigners ?? [])
        await confirmTx(connection, signature)
        setToasts((t) =>
          t.map((toast) =>
            toast.id === toastId
              ? { ...toast, kind: 'confirmed', text: `${label}: confirmed`, signature }
              : toast,
          ),
        )
        return true
      } catch (error) {
        const text = describeError(error)
        setToasts((t) =>
          t.map((toast) => (toast.id === toastId ? { ...toast, kind: 'error', text: `${label}: ${text}` } : toast)),
        )
        return false
      }
    },
    [connection, wallet],
  )

  const send = useCallback(
    async (
      label: string,
      instructions: PromiseLike<import('@solana/web3.js').TransactionInstruction>[],
      extraSigners?: Keypair[],
    ) => {
      if (!wallet || !publicKey) {
        setToasts((t) => [...t, { id: Date.now(), kind: 'error', text: 'Connect a wallet first.' }])
        return
      }
      try {
        const resolved = await Promise.all(instructions)
        if (executionMode === 'multisig') {
          const proposal: Proposal = {
            id: Date.now() + Math.floor(Math.random() * 1000),
            label,
            createdAt: Date.now(),
            createdBy: publicKey.toBase58(),
            instructions: resolved.map(toProposalInstruction),
            status: 'pending',
          }
          setProposals((prev) => persistProposals([proposal, ...prev]))
          setToasts((t) => [...t, { id: proposal.id, kind: 'pending', text: `${label}: queued for multisig approval` }])
          return
        }
        await dispatch(label, resolved, extraSigners)
      } catch (error) {
        const text = describeError(error)
        setToasts((t) => [...t, { id: Date.now(), kind: 'error', text: `${label}: ${text}` }])
      }
    },
    [wallet, publicKey, executionMode, dispatch],
  )

  const approveProposal = useCallback(
    async (id: number) => {
      if (!wallet || !publicKey) {
        setToasts((t) => [...t, { id: Date.now(), kind: 'error', text: 'Connect a wallet first.' }])
        return
      }
      const proposal = proposals.find((item) => item.id === id)
      if (!proposal) return
      let instructions: TransactionInstruction[]
      try {
        instructions = proposal.instructions.map(fromProposalInstruction)
      } catch (error) {
        setToasts((t) => [...t, { id: Date.now(), kind: 'error', text: `Proposal ${id}: ${describeError(error)}` }])
        return
      }
      const ok = await dispatch(proposal.label, instructions)
      if (ok) {
        setProposals((prev) => persistProposals(prev.map((item) => (item.id === id ? { ...item, status: 'approved' } : item))))
      }
    },
    [proposals, dispatch, wallet, publicKey],
  )

  const rejectProposal = useCallback((id: number) => {
    setProposals((prev) => persistProposals(prev.map((item) => (item.id === id ? { ...item, status: 'rejected' } : item))))
  }, [])

  const clearProposals = useCallback(() => {
    setProposals(persistProposals([]))
  }, [])

  // Auto-reconnect on page load when the provider already granted access.
  useEffect(() => {
    const provider = getInjectedWallet()
    if (!provider) return
    if (provider.publicKey && !publicKey) {
      void connect()
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  const value: AppContextValue = {
    cluster,
    setCluster,
    connection,
    wallet,
    publicKey,
    connecting,
    connect,
    disconnect,
    send,
    toasts,
    dismiss,
    role,
    setRole,
    executionMode,
    setExecutionMode,
    proposals,
    approveProposal,
    rejectProposal,
    clearProposals,
  }

  return <AppContext.Provider value={value}>{children}</AppContext.Provider>
}

export function useApp(): AppContextValue {
  const context = useContext(AppContext)
  if (!context) throw new Error('useApp must be used inside <AppProvider>')
  return context
}
