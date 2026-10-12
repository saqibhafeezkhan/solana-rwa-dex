/**
 * Default hover copy for the "i" affordance every shared component renders.
 *
 * Components use these when a caller did not pass an explicit `info`, so even an
 * unannotated page explains what each control does without a separate help screen.
 */
export const INFO_FALLBACKS = {
  panel: 'Grouped section — the controls and read-outs that belong together.',
  formRow: 'Form row — the inputs and the button that submits them together.',
  keyValue: 'Read-only key/value list of the state this section reads from chain.',
  badge: 'Status chip — the colour encodes health: good, warn or bad.',
  statusLine: 'Inline error text, shown when the last call failed.',
  emptyHint: 'Placeholder shown while there is nothing to display yet.',
  dataTable: 'Data table — one row per record; Export CSV writes what is shown.',
  sparkline: 'Line chart — each point is one reading, oldest on the left.',
  meter: 'Meter — fills as the value approaches its maximum.',
  histogram: 'Depth chart — bar height and colour both encode relative liquidity.',
  priceTileRow: 'Price tile rail — use the arrows to page to the previous or next tile.',
} as const
