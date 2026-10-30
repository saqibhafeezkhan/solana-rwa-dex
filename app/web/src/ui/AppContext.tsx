import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import type { ReactNode } from 'react'
import { type Keypair, PublicKey, TransactionInstruction } from '@solana/web3.js'
import {
  connectionFor,
  describeError,
  getInjectedWallet,
  sendWithSignersDetailed,
  confirmTx,
  type ClusterName,
  type InjectedProvider,
  type WalletLike,
} from '@rwa-dex/sdk'
import { isRole, type Role } from './enterprise'
import {
  AppContext,
  type AppContextValue,
  type ExecutionMode,
  type Proposal,
  type ProposalInstruction,
  type Toast,
} from './appContextValue'

const CLUSTER_KEY = 'rwadex.cluster'
const ROLE_KEY = 'rwadex.role'
const MODE_KEY = 'rwadex.executionMode'
const SAMPLE_MODE_KEY = 'rwadex.sampleMode'
const PROPOSALS_KEY = 'rwadex.proposals'

/** Builds the signing surface from an injected provider once it is connected. */
function walletFromProvider(provider: InjectedProvider, publicKey: PublicKey): WalletLike {
  return {
    publicKey,
    signTransaction: (tx) => {
      const sign = provider.signTransaction
      if (!sign) throw new Error('wallet cannot sign transactions')
      return sign(tx as import('@solana/web3.js').Transaction) as never
    },
  }
}

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

  const [sampleMode, setSampleModeState] = useState<boolean>(
    () =>
      // `?sample=1` is a shareable demo link; the toggle then persists normally.
      new URLSearchParams(window.location.search).get('sample') === '1' ||
      localStorage.getItem(SAMPLE_MODE_KEY) === 'true',
  )
  const setSampleMode = useCallback((enabled: boolean) => {
    localStorage.setItem(SAMPLE_MODE_KEY, String(enabled))
    setSampleModeState(enabled)
    // Switching off must also drop `?sample=1`, or a reload would turn it back on.
    if (!enabled && typeof window !== 'undefined') {
      const url = new URL(window.location.href)
      if (url.searchParams.has('sample')) {
        url.searchParams.delete('sample')
        window.history.replaceState(null, '', url)
      }
    }
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
      setWallet(walletFromProvider(provider, response.publicKey))
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

  const notify = useCallback((kind: Toast['kind'], text: string) => {
    setToasts((t) => [...t, { id: Date.now() + Math.floor(Math.random() * 1000), kind, text }])
  }, [])

  /** Broadcasts and confirms one bundle; returns whether it landed. */
  const dispatch = useCallback(
    async (label: string, resolved: TransactionInstruction[], extraSigners?: Keypair[]): Promise<boolean> => {
      const toastId = Date.now() + Math.floor(Math.random() * 1000)
      try {
        setToasts((t) => [...t, { id: toastId, kind: 'pending', text: `${label}: sending…` }])
        const sent = await sendWithSignersDetailed(connection, wallet!, resolved, extraSigners ?? [])
        await confirmTx(connection, sent.signature, sent.blockhash, sent.lastValidBlockHeight)
        setToasts((t) =>
          t.map((toast) =>
            toast.id === toastId
              ? { ...toast, kind: 'confirmed', text: `${label}: confirmed`, signature: sent.signature }
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
      instructions: (
        | import('@solana/web3.js').TransactionInstruction
        | null
        | undefined
        | PromiseLike<import('@solana/web3.js').TransactionInstruction | null | undefined>
      )[],
      extraSigners?: Keypair[],
    ) => {
      if (sampleMode) {
        setToasts((t) => [
          ...t,
          { id: Date.now(), kind: 'error', text: 'Sample data is on — switch it off to send transactions.' },
        ])
        return
      }
      if (!wallet || !publicKey) {
        setToasts((t) => [...t, { id: Date.now(), kind: 'error', text: 'Connect a wallet first.' }])
        return
      }
      try {
        const resolved = (await Promise.all(instructions)).filter(
          (instruction): instruction is import('@solana/web3.js').TransactionInstruction =>
            instruction !== null && instruction !== undefined,
        )
        if (resolved.length === 0) {
          setToasts((t) => [...t, { id: Date.now(), kind: 'error', text: `${label}: nothing to send.` }])
          return
        }
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
    [wallet, publicKey, executionMode, sampleMode, dispatch],
  )

  const approveProposal = useCallback(
    async (id: number) => {
      if (sampleMode) {
        setToasts((t) => [
          ...t,
          { id: Date.now(), kind: 'error', text: 'Sample data is on — switch it off to execute proposals.' },
        ])
        return
      }
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
    [proposals, dispatch, wallet, publicKey, sampleMode],
  )

  const rejectProposal = useCallback((id: number) => {
    setProposals((prev) => persistProposals(prev.map((item) => (item.id === id ? { ...item, status: 'rejected' } : item))))
  }, [])

  const clearProposals = useCallback(() => {
    setProposals(persistProposals([]))
  }, [])

  // Auto-reconnect on page load when the provider already granted access, and
  // follow account switches / disconnects for as long as the page is open.
  const autoConnected = useRef(false)
  useEffect(() => {
    const provider = getInjectedWallet()
    if (!provider) return
    if (provider.publicKey && !autoConnected.current) {
      autoConnected.current = true
      void connect()
    }

    const onAccountChanged = (next: PublicKey | { toString(): string } | null) => {
      if (!next) {
        setWallet(null)
        setPublicKey(null)
        return
      }
      const key = next instanceof PublicKey ? next : new PublicKey(next.toString())
      setPublicKey(key)
      setWallet(walletFromProvider(provider, key))
    }
    const onDisconnect = () => {
      setWallet(null)
      setPublicKey(null)
    }
    provider.on?.('accountChanged', onAccountChanged)
    provider.on?.('disconnect', onDisconnect)
    return () => {
      provider.off?.('accountChanged', onAccountChanged)
      provider.off?.('disconnect', onDisconnect)
    }
  }, [connect])

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
    notify,
    role,
    setRole,
    executionMode,
    setExecutionMode,
    sampleMode,
    setSampleMode,
    proposals,
    approveProposal,
    rejectProposal,
    clearProposals,
  }

  return <AppContext.Provider value={value}>{children}</AppContext.Provider>
}
