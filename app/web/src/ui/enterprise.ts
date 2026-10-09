/** Enterprise access-control and local persistence. Pure logic — no React. */

export type Role = 'trader' | 'treasury' | 'compliance' | 'admin'

export const ROLES: Role[] = ['trader', 'treasury', 'compliance', 'admin']

export const ROLE_LABELS: Record<Role, string> = {
  trader: 'Trader',
  treasury: 'Treasury Manager',
  compliance: 'Compliance Officer',
  admin: 'Admin',
}

export const ROLE_PERMISSIONS: Record<Role, string[]> = {
  trader: ['Initiate swaps and TWAMM orders', 'Queue multisig proposals'],
  treasury: ['Add / remove liquidity', 'Approve and execute proposals', 'Manage the address book'],
  compliance: ['Review the audit trail', 'Approve settlements', 'Manage the address book'],
  admin: ['Everything above', 'Pause / unpause pools', 'Rotate authorities', 'Trip the circuit breaker'],
}

export function isRole(value: unknown): value is Role {
  return typeof value === 'string' && (ROLES as string[]).includes(value)
}

export function canApproveProposals(role: Role): boolean {
  return role === 'treasury' || role === 'admin'
}

export function canManageAddressBook(role: Role): boolean {
  return role === 'treasury' || role === 'compliance' || role === 'admin'
}

export interface AddressBookEntry {
  id: number
  label: string
  address: string
  note?: string
}

const ADDRESS_BOOK_KEY = 'rwadex.addressBook'
const WHITELIST_KEY = 'rwadex.whitelist'

function readJson<T>(key: string, fallback: T): T {
  if (typeof window === 'undefined') return fallback
  try {
    const raw = localStorage.getItem(key)
    return raw ? (JSON.parse(raw) as T) : fallback
  } catch {
    return fallback
  }
}

function writeJson(key: string, value: unknown): void {
  if (typeof window === 'undefined') return
  try {
    localStorage.setItem(key, JSON.stringify(value))
  } catch {
    // storage full or unavailable — ignore
  }
}

export function loadAddressBook(): AddressBookEntry[] {
  const entries = readJson<unknown[]>(ADDRESS_BOOK_KEY, [])
  if (!Array.isArray(entries)) return []
  return entries.filter(
    (entry): entry is AddressBookEntry =>
      !!entry &&
      typeof entry === 'object' &&
      typeof (entry as AddressBookEntry).address === 'string' &&
      typeof (entry as AddressBookEntry).label === 'string',
  )
}

export function saveAddressBook(entries: AddressBookEntry[]): AddressBookEntry[] {
  writeJson(ADDRESS_BOOK_KEY, entries)
  return entries
}

/** The institution's imported Merkle-whitelist wallet set (raw base58 strings). */
export function loadWhitelist(): string[] {
  const list = readJson<unknown[]>(WHITELIST_KEY, [])
  if (!Array.isArray(list)) return []
  return list.filter((entry): entry is string => typeof entry === 'string' && entry.length > 0)
}

export function saveWhitelist(wallets: string[]): string[] {
  const unique = Array.from(new Set(wallets.map((wallet) => wallet.trim()).filter(Boolean)))
  writeJson(WHITELIST_KEY, unique)
  return unique
}
