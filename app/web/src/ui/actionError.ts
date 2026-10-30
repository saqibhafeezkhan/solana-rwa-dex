import { describeError } from '@rwa-dex/sdk'

/**
 * Turns an error thrown while *assembling* an instruction — a raw amount that is
 * not a whole number, a mistyped base58 address — into copy a user can act on.
 * Broadcast failures keep their program-error message.
 *
 * Lives apart from `components.tsx` so that file keeps exporting components only.
 */
export function describeActionError(error: unknown): string {
  if (error instanceof SyntaxError) return 'enter a whole number in every raw amount field'
  const text = describeError(error)
  return /base58|public key input/i.test(text) ? 'enter a valid base58 address' : text
}
