import { useState } from 'react'
import { PublicKey } from '@solana/web3.js'
import { useApp } from '../useApp'
import {
  ActionButton,
  Badge,
  DataTable,
  EmptyHint,
  Field,
  FormRow,
  KeyValue,
  Panel,
  TextInput,
} from '../components'
import { useAccountState } from '../useAccountState'
import { transferHookConfig } from '@rwa-dex/sdk'
import {
  ROLE_LABELS,
  ROLE_PERMISSIONS,
  ROLES,
  canApproveProposals,
  canManageAddressBook,
  loadAddressBook,
  loadWhitelist,
  saveAddressBook,
  saveWhitelist,
  type AddressBookEntry,
  type Role,
} from '../enterprise'

export function EnterprisePage() {
  const {
    connection,
    publicKey,
    role,
    setRole,
    executionMode,
    setExecutionMode,
    proposals,
    approveProposal,
    rejectProposal,
    clearProposals,
  } = useApp()

  const [entries, setEntries] = useState<AddressBookEntry[]>(() => loadAddressBook())
  const [whitelist, setWhitelist] = useState<string[]>(() => loadWhitelist())
  const [label, setLabel] = useState('')
  const [address, setAddress] = useState('')
  const [whitelistText, setWhitelistText] = useState(() => loadWhitelist().join('\n'))

  // Whether the transfer-hook config exists on this cluster (the on-chain anchor
  // for the Merkle root that this address book is meant to mirror).
  const hook = useAccountState(
    async () => {
      const info = await connection.getAccountInfo(transferHookConfig())
      return info !== null
    },
    [connection],
  )

  const validAddress = (() => {
    if (!address) return false
    try {
      new PublicKey(address)
      return true
    } catch {
      return false
    }
  })()

  const addEntry = () => {
    if (!canManageAddressBook(role) || !label.trim() || !validAddress) return
    const next = [
      { id: Date.now(), label: label.trim(), address: address.trim() },
      ...entries.filter((entry) => entry.address !== address.trim()),
    ]
    setEntries(saveAddressBook(next))
    setLabel('')
    setAddress('')
  }

  const removeEntry = (id: number) => {
    setEntries(saveAddressBook(entries.filter((entry) => entry.id !== id)))
  }

  const applyWhitelist = () => {
    const parsed = whitelistText
      .split(/[\s,;]+/)
      .map((token) => token.trim())
      .filter(Boolean)
    setWhitelist(saveWhitelist(parsed))
  }

  const whitelistStatus = (wallet: string) => {
    if (whitelist.length === 0) return <Badge tone="neutral">not checked</Badge>
    return whitelist.includes(wallet) ? (
      <Badge tone="good">in whitelist</Badge>
    ) : (
      <Badge tone="bad">not in whitelist</Badge>
    )
  }

  const copy = (value: string) => {
    void navigator.clipboard?.writeText(value)
  }

  return (
    <>
      <div className="grid-2">
        <Panel
          title="Role-based access"
          subtitle="The active role gates the enterprise surfaces. Roles are stored locally; a production deployment would bind them to multisig membership."
          aside={<Badge tone="neutral">{ROLE_LABELS[role]}</Badge>}
        >
          <FormRow>
            <Field label="Active role">
              <select className="text-input" value={role} onChange={(e) => setRole(e.target.value as Role)}>
                {ROLES.map((item) => (
                  <option key={item} value={item}>
                    {ROLE_LABELS[item]}
                  </option>
                ))}
              </select>
            </Field>
            {/* Read-outs, not inputs: same label/box band as the field beside them. */}
            <div className="field">
              <span className="field-label">Can approve proposals</span>
              <div className={`field-readout ${canApproveProposals(role) ? 'positive' : 'negative'}`}>
                {canApproveProposals(role) ? 'yes' : 'no'}
              </div>
            </div>
            <div className="field">
              <span className="field-label">Can manage address book</span>
              <div className={`field-readout ${canManageAddressBook(role) ? 'positive' : 'negative'}`}>
                {canManageAddressBook(role) ? 'yes' : 'no'}
              </div>
            </div>
          </FormRow>
          <DataTable
            rows={ROLES}
            empty="No roles configured."
            columns={[
              {
                header: 'Role',
                cell: (row) => (
                  <span className={row === role ? 'mono' : undefined}>
                    {ROLE_LABELS[row]}
                    {row === role ? ' (active)' : ''}
                  </span>
                ),
                text: (row) => ROLE_LABELS[row],
              },
              {
                header: 'Capabilities',
                cell: (row) => ROLE_PERMISSIONS[row].join(' · '),
                text: (row) => ROLE_PERMISSIONS[row].join(' · '),
              },
            ]}
          />
        </Panel>

        <Panel
          title="Execution mode"
          subtitle="Direct signs and broadcasts from the connected wallet. Multisig captures every action as a proposal for an approver to execute."
          aside={<Badge tone={executionMode === 'multisig' ? 'warn' : 'good'}>{executionMode}</Badge>}
        >
          <div className="viz-mode-tabs">
            <button
              type="button"
              className={`viz-mode-tab ${executionMode === 'direct' ? 'is-active' : ''}`}
              onClick={() => setExecutionMode('direct')}
            >
              Direct (wallet)
            </button>
            <button
              type="button"
              className={`viz-mode-tab ${executionMode === 'multisig' ? 'is-active' : ''}`}
              onClick={() => setExecutionMode('multisig')}
            >
              Multisig proposal
            </button>
          </div>
          <KeyValue
            rows={[
              ['Queued proposals', proposals.filter((proposal) => proposal.status === 'pending').length.toString()],
              ['Connected wallet', publicKey ? publicKey.toBase58() : '—'],
            ]}
          />
          <p className="viz-note">
            Proposals store their instruction payloads locally (base64) so they survive a reload. Approving rebuilds the
            transaction and broadcasts it; wiring to a Squads vault would submit this payload to the multisig instead.
            Actions that need an extra keypair signer (e.g. creating a mint) cannot be staged and stay direct-only.
          </p>
        </Panel>
      </div>

      <Panel
        title="Multisig proposal queue"
        subtitle="Actions captured while in multisig mode. An approver with the Treasury Manager or Admin role executes them."
        aside={
          <ActionButton
            label="Clear queue"
            variant="secondary"
            onClick={clearProposals}
            disabled={proposals.length === 0}
          />
        }
      >
        <DataTable
          csvName="rwadex-proposals"
          empty="No proposals queued. Switch to multisig mode and perform an action."
          rows={proposals}
          columns={[
            { header: 'Id', cell: (row) => row.id.toString(), text: (row) => row.id.toString() },
            { header: 'Action', cell: (row) => row.label, text: (row) => row.label },
            {
              header: 'Instructions',
              cell: (row) => row.instructions.length.toString(),
              text: (row) => row.instructions.length.toString(),
            },
            {
              header: 'Created',
              cell: (row) => new Date(row.createdAt).toLocaleString(),
              text: (row) => new Date(row.createdAt).toISOString(),
            },
            {
              header: 'Status',
              cell: (row) => (
                <Badge tone={row.status === 'pending' ? 'warn' : row.status === 'approved' ? 'good' : 'bad'}>
                  {row.status}
                </Badge>
              ),
              text: (row) => row.status,
            },
            {
              header: 'Actions',
              cell: (row) =>
                row.status !== 'pending' ? (
                  '—'
                ) : (
                  <span className="proposal-actions">
                    <button
                      className="btn btn-secondary btn-compact"
                      onClick={() => void approveProposal(row.id)}
                      disabled={!canApproveProposals(role)}
                      title={canApproveProposals(role) ? 'Approve and execute' : `Role ${ROLE_LABELS[role]} cannot approve`}
                    >
                      Approve
                    </button>
                    <button className="btn btn-danger btn-compact" onClick={() => rejectProposal(row.id)}>
                      Reject
                    </button>
                  </span>
                ),
              text: () => '',
            },
          ]}
        />
      </Panel>

      <Panel
        title="Counterparty address book"
        subtitle="Save counterparties and check them against your imported whitelist before transferring. The transfer hook requires a Merkle proof for both wallets."
        aside={
          hook.state === null ? null : hook.state ? (
            <Badge tone="good">hook initialized</Badge>
          ) : (
            <Badge tone="bad">hook not initialized</Badge>
          )
        }
      >
        <FormRow>
          <Field label="Label">
            <TextInput value={label} onChange={(e) => setLabel(e.target.value)} placeholder="e.g. Custodian B" />
          </Field>
          <Field label="Wallet address (base58)">
            <TextInput value={address} onChange={(e) => setAddress(e.target.value)} placeholder="Base58 pubkey" />
          </Field>
          <ActionButton
            label="Add / update"
            onClick={addEntry}
            disabled={!canManageAddressBook(role) || !label.trim() || !validAddress}
          />
        </FormRow>

        <Field
          label="Whitelist import (one wallet per line)"
          hint="Paste the off-chain Merkle whitelist to check membership locally. Proof generation needs the full tree."
        >
          <textarea
            className="text-input"
            value={whitelistText}
            onChange={(e) => setWhitelistText(e.target.value)}
            rows={4}
            aria-label="Whitelist wallets, one per line"
            placeholder="WalletA&#10;WalletB"
          />
        </Field>
        <FormRow>
          <ActionButton
            label="Apply whitelist"
            variant="secondary"
            onClick={applyWhitelist}
            disabled={!canManageAddressBook(role)}
          />
          <ActionButton
            label="Clear whitelist"
            variant="secondary"
            onClick={() => {
              setWhitelistText('')
              setWhitelist(saveWhitelist([]))
            }}
            disabled={!canManageAddressBook(role)}
          />
        </FormRow>

        <DataTable
          csvName="rwadex-address-book"
          empty="No counterparties saved yet."
          rows={entries}
          columns={[
            { header: 'Label', cell: (row) => row.label, text: (row) => row.label },
            {
              header: 'Address',
              cell: (row) => (
                <span title={row.address} className="mono">
                  {row.address.slice(0, 6)}…{row.address.slice(-6)}
                </span>
              ),
              text: (row) => row.address,
            },
            {
              header: 'Whitelist',
              cell: (row) => whitelistStatus(row.address),
              text: (row) => (whitelist.length === 0 ? 'not checked' : whitelist.includes(row.address) ? 'in whitelist' : 'not in whitelist'),
            },
            {
              header: 'Actions',
              cell: (row) => (
                <span className="proposal-actions">
                  <button className="btn btn-secondary btn-compact" onClick={() => copy(row.address)}>
                    Copy
                  </button>
                  <button
                    className="btn btn-danger btn-compact"
                    onClick={() => removeEntry(row.id)}
                    disabled={!canManageAddressBook(role)}
                  >
                    Remove
                  </button>
                </span>
              ),
              text: () => '',
            },
          ]}
        />
        {entries.length === 0 ? <EmptyHint>Add a counterparty above to build the book.</EmptyHint> : null}
      </Panel>
    </>
  )
}
