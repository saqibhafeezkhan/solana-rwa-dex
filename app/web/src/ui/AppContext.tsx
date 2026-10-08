import { createContext, useCallback, useContext, useEffect, useMemo, useState } from 'react'
import type { ReactNode } from 'react'
import { Keypair, PublicKey } from '@solana/web3.js'
import {
  connectionFor,
  describeError,
  getInjectedWallet,
  sendWithSigners,
  confirmTx,
  type ClusterName,
  type WalletLike,
} from '../core/connection'

interface Toast {
  id: number
  kind: 'pending' | 'confirmed' | 'error'
  text: string
  signature?: string
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
  /** Sends + confirms one instruction bundle and reports via toasts. */
  send: (
    label: string,
    instructions: PromiseLike<import('@solana/web3.js').TransactionInstruction>[],
    extraSigners?: Keypair[],
  ) => Promise<void>
  toasts: Toast[]
  dismiss: (id: number) => void
}

const AppContext = createContext<AppContextValue | null>(null)

const CLUSTER_KEY = 'rwadex.cluster'

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
      const toastId = Date.now() + Math.floor(Math.random() * 1000)
      try {
        const resolved = await Promise.all(instructions)
        setToasts((t) => [...t, { id: toastId, kind: 'pending', text: `${label}: sending…` }])
        const signature = await sendWithSigners(connection, wallet, resolved, extraSigners ?? [])
        await confirmTx(connection, signature)
        setToasts((t) =>
          t.map((toast) =>
            toast.id === toastId
              ? { ...toast, kind: 'confirmed', text: `${label}: confirmed`, signature }
              : toast,
          ),
        )
      } catch (error) {
        const text = describeError(error)
        setToasts((t) =>
          t.map((toast) => (toast.id === toastId ? { ...toast, kind: 'error', text: `${label}: ${text}` } : toast)),
        )
      }
    },
    [connection, wallet, publicKey],
  )

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
  }

  return <AppContext.Provider value={value}>{children}</AppContext.Provider>
}

export function useApp(): AppContextValue {
  const context = useContext(AppContext)
  if (!context) throw new Error('useApp must be used inside <AppProvider>')
  return context
}
