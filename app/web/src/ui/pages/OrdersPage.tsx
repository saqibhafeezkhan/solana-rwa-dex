import { useMemo, useState } from 'react'
import { PublicKey } from '@solana/web3.js'
import { useApp } from '../useApp'
import {
  ActionButton,
  Badge,
  DataTable,
  Field,
  FormRow,
  KeyValue,
  NumberInput,
  Panel,
  Select,
  StatusLine,
  TextInput,
} from '../components'
import { describeActionError } from '../actionError'
import { useAccountState } from '../useAccountState'
import { useSampleMints } from '../useSampleMints'
import { useTokenProgram } from '../useTokenProgram'
import {
  ensureAtaInstruction,
  hookAccountsForTransfers,
  shortAddress,
} from '../../core/connection'
import {
  ORDER_LEN,
  decodeOrder,
  loadKycConfig,
  loadMint,
  loadOrderBook,
  type OrderView,
  type OrderBookState,
} from '../../core/decoders'
import {
  ORDER_BOOK_PROGRAM_ID,
  hookAuthority,
  kycConfig,
  orderBook,
  orderBookOrder,
  settlementEscrowAuthority,
} from '../../core/pdas'
import {
  orderBookInitialize,
  orderBookKycAttestation,
  orderBookCreateOrder,
  orderBookFillOrder,
  orderBookCancelOrder,
  randomTradeId,
  type OrderSide,
} from '../../core/instructions-orderbook'
import { SAMPLE_KYC_CONFIG, SAMPLE_ORDER_BOOK, SAMPLE_ORDERS } from '../../core/sampleData'

const PRICE_SCALE = 1_000_000_000n

type HookAccount = { pubkey: PublicKey; isSigner: boolean; isWritable: boolean }

/** Deduplicates the hook-account lists for the transfers one instruction makes. */
async function mergeHookAccounts(
  ...lists: (HookAccount[] | undefined)[]
): Promise<HookAccount[] | undefined> {
  const seen = new Set<string>()
  const merged: HookAccount[] = []
  for (const list of lists) {
    for (const account of list ?? []) {
      const key = account.pubkey.toBase58()
      if (seen.has(key)) continue
      seen.add(key)
      merged.push(account)
    }
  }
  return merged.length > 0 ? merged : undefined
}

/** Human price -> the on-chain quote-per-base price, scaled by 1e9. */
function toRawPrice(human: string, baseDecimals: number, quoteDecimals: number): bigint {
  const value = Number(human)
  if (!Number.isFinite(value) || value <= 0) throw new Error('enter a positive price')
  const scaled = value * 10 ** (quoteDecimals - baseDecimals) * 1e9
  const raw = BigInt(Math.round(scaled))
  if (raw <= 0n) throw new Error('price is too small')
  return raw
}

/** On-chain price -> a human quote-per-base price. */
function fromRawPrice(price: bigint, baseDecimals: number, quoteDecimals: number): string {
  const value = (Number(price) / 1e9) * 10 ** (baseDecimals - quoteDecimals)
  return value.toLocaleString(undefined, { maximumFractionDigits: 6 })
}

/** Quote owed for a base amount, rounded up, as the program does. */
function quoteForBase(base: bigint, price: bigint): bigint {
  return (base * price + PRICE_SCALE - 1n) / PRICE_SCALE
}

/** Base bought for a quote amount, rounded down, as the program does. */
function baseForQuote(quote: bigint, price: bigint): bigint {
  return (quote * PRICE_SCALE) / price
}

export function OrdersPage() {
  const { connection, publicKey, send, sampleMode } = useApp()
  const { mintA, setMintA, mintB, setMintB } = useSampleMints(
    { mintA: SAMPLE_ORDER_BOOK.mintBase, mintB: SAMPLE_ORDER_BOOK.mintQuote },
    sampleMode,
  )

  const baseMint = useMemo(() => {
    try {
      return mintA ? new PublicKey(mintA) : null
    } catch {
      return null
    }
  }, [mintA])
  const quoteMint = useMemo(() => {
    try {
      return mintB ? new PublicKey(mintB) : null
    } catch {
      return null
    }
  }, [mintB])

  const bookAddress = useMemo(
    () => (baseMint && quoteMint ? orderBook(baseMint, quoteMint) : null),
    [baseMint, quoteMint],
  )

  const book = useAccountState(
    async (): Promise<OrderBookState | null> => {
      if (sampleMode) return SAMPLE_ORDER_BOOK
      if (!bookAddress) return null
      return loadOrderBook(connection, bookAddress)
    },
    [connection, bookAddress?.toBase58(), sampleMode],
  )

  // Decimals are only used for display and for the human price input. Both
  // mints normally share a token program on an institutional pair.
  const mintInfo = useAccountState(
    async () => {
      if (!baseMint || !quoteMint) return null
      if (sampleMode) return { base: 6, quote: 6 }
      const [base, quote] = await Promise.all([
        loadMint(connection, baseMint),
        loadMint(connection, quoteMint),
      ])
      return { base: base.decimals, quote: quote.decimals }
    },
    [connection, baseMint?.toBase58(), quoteMint?.toBase58(), sampleMode],
  )
  const baseDecimals = mintInfo.state?.base ?? 6
  const quoteDecimals = mintInfo.state?.quote ?? 6

  const baseTokenProgramId = useTokenProgram(baseMint)
  const quoteTokenProgramId = useTokenProgram(quoteMint)

  // ---- initialize book ------------------------------------------------
  const [initKyc, setInitKyc] = useState(false)
  const [initSettlement, setInitSettlement] = useState(false)
  const [initSlots, setInitSlots] = useState('100')
  const [initError, setInitError] = useState<string | null>(null)

  const initializeBook = async () => {
    try {
      setInitError(null)
      if (!publicKey) throw new Error('connect a wallet first')
      if (!baseMint || !quoteMint) throw new Error('enter both mints')
      const ix = await orderBookInitialize({
        authority: publicKey,
        baseMint,
        quoteMint,
        kycConfig: initKyc ? kycConfig() : null,
        hooksAuthority: initSettlement ? hookAuthority() : null,
        settlementSlots: initSettlement ? BigInt(initSlots) : 0n,
      })
      await send('Initialize order book', [ix])
      book.refresh()
    } catch (error) {
      setInitError(describeActionError(error))
    }
  }

  const kyc = useAccountState(
    async () => {
      if (!book.state) return null
      if (book.state.kycConfig.equals(PublicKey.default)) return null
      if (sampleMode) return SAMPLE_KYC_CONFIG
      return loadKycConfig(connection, book.state.kycConfig)
    },
    [connection, book.state?.kycConfig.toBase58(), sampleMode],
  )

  const orders = useAccountState(
    async (): Promise<OrderView[]> => {
      if (sampleMode) return SAMPLE_ORDERS
      if (!bookAddress) return []
      const rows = await connection.getProgramAccounts(ORDER_BOOK_PROGRAM_ID, {
        filters: [
          { dataSize: ORDER_LEN },
          { memcmp: { offset: 8, bytes: bookAddress.toBase58() } },
        ],
        encoding: 'base64',
      })
      const decoded: OrderView[] = []
      for (const row of rows) {
        try {
          const info = await connection.getAccountInfo(row.pubkey)
          if (!info) continue
          decoded.push(
            decodeOrder({
              pubkey: row.pubkey,
              owner: info.owner,
              data: info.data,
              lamports: info.lamports,
            }),
          )
        } catch {
          // A partially written or foreign account is simply not listed.
        }
      }
      return decoded.sort((a, b) => Number(b.createdAt - a.createdAt))
    },
    [connection, bookAddress?.toBase58(), sampleMode],
  )

  const settlementOn = book.state ? !book.state.hooksAuthority.equals(PublicKey.default) : false
  const kycOn = book.state ? !book.state.kycConfig.equals(PublicKey.default) : false
  const kycMint = kyc.state?.kycMint ?? null
  const kycAccounts =
    kycOn && kycMint && book.state
      ? {
          config: book.state.kycConfig,
          attestation: orderBookKycAttestation(publicKey ?? PublicKey.default, kycMint),
        }
      : undefined

  // ---- place order ---------------------------------------------------
  const [side, setSide] = useState<OrderSide>('ask')
  const [price, setPrice] = useState('2.0')
  const [amount, setAmount] = useState('1000')
  const [expiryMinutes, setExpiryMinutes] = useState('1440')
  // Each order is a PDA keyed by this id, so reusing one collides with the
  // existing order. Start from the clock and advance after every placement.
  const [orderId, setOrderId] = useState(() => String(Date.now()))
  const [createError, setCreateError] = useState<string | null>(null)

  const placeOrder = async () => {
    try {
      setCreateError(null)
      if (!publicKey) throw new Error('connect a wallet first')
      if (!baseMint || !quoteMint) throw new Error('enter both mints')
      if (!book.state) throw new Error('the book is not initialized for this pair')
      const rawPrice = toRawPrice(price, baseDecimals, quoteDecimals)
      const amountIn = BigInt(amount)
      if (amountIn <= 0n) throw new Error('amount must be positive')
      const expiry =
        BigInt(Math.floor(Date.now() / 1000)) + BigInt(expiryMinutes) * 60n
      if (expiry <= BigInt(Math.floor(Date.now() / 1000))) {
        throw new Error('expiry must be in the future')
      }
      const escrowMint = side === 'ask' ? baseMint : quoteMint
      const escrowTokenProgram =
        side === 'ask' ? baseTokenProgramId : quoteTokenProgramId
      const order = orderBookOrder(book.state.address, publicKey, BigInt(orderId))
      const hookAccounts = sampleMode
        ? undefined
        : await hookAccountsForTransfers(connection, [escrowMint], [publicKey], [order])
      const ix = await orderBookCreateOrder({
        maker: publicKey,
        baseMint,
        quoteMint,
        orderId: BigInt(orderId),
        side,
        price: rawPrice,
        amount: amountIn,
        expiryTimestamp: expiry,
        escrowMint,
        tokenProgramId: escrowTokenProgram,
        kyc: kycAccounts,
        hookAccounts,
      })
      const ensureMakerAta = await ensureAtaInstruction(
        connection,
        publicKey,
        escrowMint,
        publicKey,
        escrowTokenProgram,
      )
      await send(`Place ${side} ${amountIn}`, [ensureMakerAta, ix])
      setOrderId(String(Date.now()))
      orders.refresh()
    } catch (error) {
      setCreateError(describeActionError(error))
    }
  }

  // ---- fill ----------------------------------------------------------
  const [selected, setSelected] = useState<OrderView | null>(null)
  const [fillAmount, setFillAmount] = useState('')
  const [maxCounter, setMaxCounter] = useState('')
  const [fillError, setFillError] = useState<string | null>(null)

  const selectOrder = (order: OrderView) => {
    setSelected(order)
    setFillAmount(String(order.remainingAmount))
    const counter =
      order.side === 'ask'
        ? quoteForBase(order.remainingAmount, order.price)
        : baseForQuote(order.remainingAmount, order.price)
    setMaxCounter(String(counter))
    setFillError(null)
  }

  const fill = async () => {
    try {
      setFillError(null)
      if (!publicKey) throw new Error('connect a wallet first')
      if (!book.state || !selected) throw new Error('select an order first')
      const amountIn = BigInt(fillAmount)
      if (amountIn <= 0n) throw new Error('amount must be positive')
      if (amountIn > selected.remainingAmount) throw new Error('more than the order has left')
      if (!baseMint || !quoteMint) throw new Error('enter both mints')
      const escrowMint = selected.side === 'ask' ? baseMint : quoteMint
      const paymentMint = selected.side === 'ask' ? quoteMint : baseMint
      const orderAddress = orderBookOrder(book.state.address, selected.owner, selected.orderId)
      const hookAccounts = sampleMode
        ? undefined
        : await mergeHookAccounts(
            await hookAccountsForTransfers(
              connection,
              [paymentMint],
              [publicKey],
              [settlementOn ? hookAuthority() : selected.owner],
            ),
            await hookAccountsForTransfers(connection, [escrowMint], [orderAddress], [publicKey]),
            settlementOn
              ? await hookAccountsForTransfers(
                  connection,
                  [paymentMint],
                  [hookAuthority()],
                  [settlementEscrowAuthority()],
                )
              : undefined,
          )
      const ix = await orderBookFillOrder({
        taker: publicKey,
        maker: selected.owner,
        baseMint,
        quoteMint,
        orderId: selected.orderId,
        side: selected.side,
        amount: amountIn,
        maxCounterAmount: BigInt(maxCounter),
        tradeId: settlementOn ? randomTradeId() : 0n,
        baseTokenProgramId,
        quoteTokenProgramId,
        settlement: settlementOn,
        kyc: kycAccounts,
        hookAccounts,
      })
      const paymentTokenProgram =
        selected.side === 'ask' ? quoteTokenProgramId : baseTokenProgramId
      const ensureTakerBase = await ensureAtaInstruction(
        connection,
        publicKey,
        baseMint,
        publicKey,
        baseTokenProgramId,
      )
      const ensureTakerQuote = await ensureAtaInstruction(
        connection,
        publicKey,
        quoteMint,
        publicKey,
        quoteTokenProgramId,
      )
      const ensureProceeds = await ensureAtaInstruction(
        connection,
        settlementOn ? hookAuthority() : selected.owner,
        paymentMint,
        publicKey,
        paymentTokenProgram,
      )
      await send(`Fill order ${selected.orderId}`, [
        ensureTakerBase,
        ensureTakerQuote,
        ensureProceeds,
        ix,
      ])
      setSelected(null)
      orders.refresh()
      book.refresh()
    } catch (error) {
      setFillError(describeActionError(error))
    }
  }

  const cancel = async (order: OrderView) => {
    try {
      if (!publicKey) throw new Error('connect a wallet first')
      if (!baseMint || !quoteMint) throw new Error('enter both mints')
      const escrowMint = order.side === 'ask' ? baseMint : quoteMint
      const escrowTokenProgram =
        order.side === 'ask' ? baseTokenProgramId : quoteTokenProgramId
      const orderAddress = orderBookOrder(bookAddress ?? order.book, publicKey, order.orderId)
      const hookAccounts = sampleMode
        ? undefined
        : await hookAccountsForTransfers(connection, [escrowMint], [orderAddress], [publicKey])
      const ix = await orderBookCancelOrder({
        maker: publicKey,
        baseMint,
        quoteMint,
        orderId: order.orderId,
        side: order.side,
        tokenProgramId: escrowTokenProgram,
        hookAccounts,
      })
      const ensureMakerAta = await ensureAtaInstruction(
        connection,
        publicKey,
        escrowMint,
        publicKey,
        escrowTokenProgram,
      )
      await send(`Cancel order ${order.orderId}`, [ensureMakerAta, ix])
      orders.refresh()
    } catch (error) {
      setFillError(describeActionError(error))
    }
  }

  const canCancel = (order: OrderView) =>
    publicKey !== null && order.owner.equals(publicKey) && !sampleMode

  return (
    <>
      <div className="page-heading">
        <div>
          <h2>Limit orders</h2>
          <p className="panel-subtitle">
            Resting maker orders with partial fills, compliance-gated and optionally routed
            through the T+N clearing rail.
          </p>
        </div>
        {book.state ? (
          <Badge tone={book.state.paused ? 'bad' : 'good'}>
            {book.state.paused ? 'paused' : 'open'}
          </Badge>
        ) : null}
      </div>

      <Panel title="Market" subtitle="The book is keyed by its base/quote pair.">
        <FormRow>
          <Field
            label="Base mint"
            hint={
              sampleMode
                ? 'Sample mode shows the demo book’s mints — switch it off to enter your own.'
                : 'The asset being bought and sold.'
            }
          >
            <TextInput value={mintA} onChange={(event) => setMintA(event.target.value)} readOnly={sampleMode} />
          </Field>
          <Field label="Quote mint" hint="The pricing asset, e.g. USDC.">
            <TextInput value={mintB} onChange={(event) => setMintB(event.target.value)} readOnly={sampleMode} />
          </Field>
        </FormRow>
        <StatusLine error={book.error} />
        {!book.state && baseMint && quoteMint && !sampleMode ? (
          <>
            <p className="panel-subtitle">
              This pair has no book yet. Only the program's upgrade authority can create one.
            </p>
            <FormRow>
              <Field label="KYC rail" hint="Verify makers and takers before value moves.">
                <Select
                  value={initKyc ? 'on' : 'off'}
                  onChange={(event) => setInitKyc(event.target.value === 'on')}
                >
                  <option value="off">Off</option>
                  <option value="on">Require attestation</option>
                </Select>
              </Field>
              <Field label="Settlement rail" hint="Route maker proceeds into T+N escrow.">
                <Select
                  value={initSettlement ? 'on' : 'off'}
                  onChange={(event) => setInitSettlement(event.target.value === 'on')}
                >
                  <option value="off">Direct payout</option>
                  <option value="on">T+N escrow</option>
                </Select>
              </Field>
              <Field label="T+ slots" hint="Used when the settlement rail is on.">
                <NumberInput
                  value={initSlots}
                  onChange={(event) => setInitSlots(event.target.value)}
                />
              </Field>
            </FormRow>
            <ActionButton label="Initialize book" onClick={() => void initializeBook()} />
            <StatusLine error={initError} />
          </>
        ) : null}
        {book.state ? (
          <KeyValue
            rows={[
              [
                'Book',
                <span key="book" className="mono">
                  {shortAddress(book.state.address)}
                </span>,
              ],
              [
                'Authority',
                <span key="authority" className="mono">
                  {shortAddress(book.state.authority)}
                </span>,
              ],
              [
                'KYC rail',
                <Badge key="kyc" tone={kycOn ? 'good' : 'neutral'}>
                  {kycOn ? 'required' : 'off'}
                </Badge>,
              ],
              [
                'Settlement rail',
                <Badge key="settlement" tone={settlementOn ? 'good' : 'neutral'}>
                  {settlementOn ? `T+${book.state.settlementSlots} slots` : 'direct payout'}
                </Badge>,
              ],
              ['Orders listed', String(orders.state?.length ?? 0)],
            ]}
          />
        ) : null}
      </Panel>

      <Panel title="Place a limit order" subtitle="The input is escrowed until filled or cancelled.">
        <FormRow>
          <Field label="Side" hint="An ask sells base; a bid buys base with quote.">
            <Select value={side} onChange={(event) => setSide(event.target.value as OrderSide)}>
              <option value="ask">Ask — sell base</option>
              <option value="bid">Bid — buy base</option>
            </Select>
          </Field>
          <Field
            label="Price"
            hint={`Quote per base (decimals ${quoteDecimals}/${baseDecimals}).`}
          >
            <NumberInput value={price} onChange={(event) => setPrice(event.target.value)} />
          </Field>
          <Field label="Amount" hint="In the escrowed token: base for an ask, quote for a bid.">
            <NumberInput value={amount} onChange={(event) => setAmount(event.target.value)} />
          </Field>
        </FormRow>
        <FormRow>
          <Field label="Expiry (minutes)" hint="Fills stop at expiry; cancellation stays open.">
            <NumberInput
              value={expiryMinutes}
              onChange={(event) => setExpiryMinutes(event.target.value)}
            />
          </Field>
          <Field label="Order id" hint="Unique per maker and book.">
            <NumberInput value={orderId} onChange={(event) => setOrderId(event.target.value)} />
          </Field>
          <div className="field">
            <span className="field-label">&nbsp;</span>
            <ActionButton
              label={sampleMode ? 'Sample data — sending is off' : 'Place order'}
              onClick={() => void placeOrder()}
              disabled={sampleMode || !book.state}
            />
          </div>
        </FormRow>
        <StatusLine error={createError} />
      </Panel>

      <Panel
        title="Resting orders"
        subtitle="Fill against any order; cancel your own at any time."
        aside={
          <ActionButton label="Refresh" variant="secondary" onClick={() => orders.refresh()} />
        }
      >
        <StatusLine error={orders.error} />
        <DataTable
          csvName="rwadex-limit-orders"
          empty="No resting orders on this book."
          rows={orders.state ?? []}
          columns={[
            {
              header: 'Side',
              cell: (row) => (
                <Badge tone={row.side === 'ask' ? 'warn' : 'good'}>{row.side}</Badge>
              ),
              text: (row) => row.side,
            },
            {
              header: 'Price',
              cell: (row) => fromRawPrice(row.price, baseDecimals, quoteDecimals),
              text: (row) => fromRawPrice(row.price, baseDecimals, quoteDecimals),
            },
            {
              header: 'Remaining',
              cell: (row) => row.remainingAmount.toString(),
              text: (row) => row.remainingAmount.toString(),
            },
            {
              header: 'Filled (base / quote)',
              cell: (row) => `${row.filledBase} / ${row.filledQuote}`,
              text: (row) => `${row.filledBase} / ${row.filledQuote}`,
            },
            {
              header: 'Owner',
              cell: (row) => <span className="mono">{shortAddress(row.owner)}</span>,
              text: (row) => row.owner.toBase58(),
            },
            {
              header: 'Expiry',
              cell: (row) => new Date(Number(row.expiryTimestamp) * 1000).toLocaleString(),
              text: (row) => row.expiryTimestamp.toString(),
            },
            {
              header: 'Action',
              cell: (row) => (
                <div className="button-row">
                  <ActionButton
                    label="Fill"
                    variant="secondary"
                    onClick={() => selectOrder(row)}
                  />
                  {canCancel(row) ? (
                    <ActionButton
                      label="Cancel"
                      variant="danger"
                      onClick={() => void cancel(row)}
                    />
                  ) : null}
                </div>
              ),
            },
          ]}
        />
      </Panel>

      {selected ? (
        <Panel
          title={`Fill order ${selected.orderId}`}
          subtitle="The amount is in the order's escrow token. Max counter-leg is your slippage bound."
        >
          <p className="panel-subtitle">
            {selected.remainingAmount.toString()} left ({selected.side}).
          </p>
          <FormRow>
            <Field label="Amount" hint="Must not exceed the remaining escrow.">
              <NumberInput
                value={fillAmount}
                onChange={(event) => setFillAmount(event.target.value)}
              />
            </Field>
            <Field
              label="Max pay"
              hint={selected.side === 'ask' ? 'Quote you will pay.' : 'Base you will pay.'}
            >
              <NumberInput
                value={maxCounter}
                onChange={(event) => setMaxCounter(event.target.value)}
              />
            </Field>
            <div className="field">
              <span className="field-label">&nbsp;</span>
              <ActionButton
                label={sampleMode ? 'Sample data — sending is off' : 'Fill order'}
                onClick={() => void fill()}
                disabled={sampleMode}
              />
            </div>
          </FormRow>
          <StatusLine error={fillError} />
        </Panel>
      ) : null}
    </>
  )
}
