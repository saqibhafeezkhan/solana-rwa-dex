/**
 * On-chain walk derivation.
 *
 * Both the TWAMM order list and the Maverick move chain are linked structures on
 * chain whose members are only discoverable by following the links. Clients
 * previously asked the operator to type the resulting account lists by hand;
 * these helpers read the links instead, so the lists always match the state the
 * program will actually walk.
 *
 * Every walk is bounded so a corrupt or unexpectedly long chain can never spin
 * the tab: the on-chain programs apply their own limits, and these caps are the
 * client-side mirror of that discipline.
 */
import { PublicKey, type Connection } from '@solana/web3.js'
import {
  MAVERICK_BINS_PER_ARRAY,
  maverickBinArray,
  maverickBinArrayIndex,
} from './pdas'
import { decodeTwammOrder, loadMaverickBinArray } from './decoders'

/** Hard cap on how many links a single walk will follow. */
export const WALK_LIMIT = 64

const ZERO = PublicKey.default

async function readOrder(connection: Connection, key: PublicKey) {
  const info = await connection.getAccountInfo(key)
  if (!info) throw new Error(`order ${key.toBase58()} not found`)
  return decodeTwammOrder({ pubkey: key, owner: info.owner, data: info.data, lamports: info.lamports })
}

/**
 * `create_long_term_order`: the live-and-expired orders from `order_head` up to
 * (not including) the first order expiring after `newExpiry`, in list order.
 *
 * The on-chain insert walk settles every expired order it is handed and stops
 * at the last order whose expiry is `<= newExpiry`; supplying exactly this range
 * satisfies both, and its final check (`tail == 0 || tail > newExpiry`) then
 * holds because the walk stopped on the first later expiry (or at the end).
 */
export async function deriveTwammInsertionWalk(
  connection: Connection,
  orderHead: PublicKey,
  newExpiry: bigint,
): Promise<PublicKey[]> {
  const walk: PublicKey[] = []
  let cursor = orderHead
  while (!cursor.equals(ZERO) && walk.length < WALK_LIMIT) {
    const order = await readOrder(connection, cursor)
    if (order.expiryTimestamp > newExpiry) break
    walk.push(cursor)
    cursor = order.next
  }
  return walk
}

/**
 * `execute_virtual_orders`: every order at the head whose expiry has passed, in
 * list order. The program refuses to leave an expired order at the head, so the
 * crank must carry all of them (or pass none when the head is still live).
 */
export async function deriveTwammExpiredWalk(
  connection: Connection,
  orderHead: PublicKey,
  now: bigint,
): Promise<PublicKey[]> {
  const walk: PublicKey[] = []
  let cursor = orderHead
  while (!cursor.equals(ZERO) && walk.length < WALK_LIMIT) {
    const order = await readOrder(connection, cursor)
    if (order.expiryTimestamp > now) break
    walk.push(cursor)
    cursor = order.next
  }
  return walk
}

/**
 * `cancel_order`: the orders in front of `target` (from `order_head` up to but
 * excluding it). The unlink walk requires the caller's list to end exactly on
 * the order being cancelled, so this throws when `target` is not in the list
 * rather than sending a walk the program would reject.
 */
export async function deriveTwammPrefixWalk(
  connection: Connection,
  orderHead: PublicKey,
  target: PublicKey,
): Promise<PublicKey[]> {
  const walk: PublicKey[] = []
  let cursor = orderHead
  while (!cursor.equals(ZERO) && walk.length < WALK_LIMIT) {
    if (cursor.equals(target)) return walk
    const order = await readOrder(connection, cursor)
    walk.push(cursor)
    cursor = order.next
  }
  if (cursor.equals(target)) return walk
  throw new Error('order is not in the pool\u2019s live order list')
}

/**
 * `maverick::remove_liquidity`: the bin arrays the position\u2019s move chain runs
 * through, excluding the position\u2019s own array (already the second account of
 * the instruction). A transaction cannot carry the same account twice, so the
 * home array must not appear again here.
 *
 * The chain is monotonic by construction (each hop advances toward the active
 * bin), so a bounded loop is enough; the program re-checks monotonicity itself.
 */
export async function deriveMaverickHopArrayIndexes(
  connection: Connection,
  pool: PublicKey,
  binId: number,
): Promise<bigint[]> {
  const homeIndex = maverickBinArrayIndex(binId)
  const seen = new Set<string>()
  const hops: bigint[] = []
  let cursor = binId

  for (let hop = 0; hop < WALK_LIMIT; hop++) {
    const arrayIndex = maverickBinArrayIndex(cursor)
    const array = await loadMaverickBinArray(connection, maverickBinArray(pool, arrayIndex))
    const offset = ((cursor % MAVERICK_BINS_PER_ARRAY) + MAVERICK_BINS_PER_ARRAY) % MAVERICK_BINS_PER_ARRAY
    const bin = array.bins[offset]
    if (!bin || !bin.moved) break

    const nextIndex = maverickBinArrayIndex(bin.toBinId)
    const key = nextIndex.toString()
    if (nextIndex !== homeIndex && !seen.has(key)) {
      seen.add(key)
      hops.push(nextIndex)
    }
    cursor = bin.toBinId
  }

  return hops
}
