/**
 * Post-processes the emitted declarations for NodeNext consumers.
 *
 * `tsc` writes our extensionless relative imports verbatim into `dist/types`
 * (`export * from './core/pdas'`). Node's ESM resolver — and therefore any
 * consumer with `module: "nodenext"` — requires an explicit extension, so those
 * declarations fail to resolve even though the bundled `dist/index.js` is fine.
 *
 * This walks `dist/types` and appends `.js` to every relative specifier (both
 * `from '…'` and `import('…')`). TypeScript maps `./x.js` back to `./x.d.ts`, so
 * bundler-resolution consumers keep working too.
 */
import { readdirSync, readFileSync, statSync, writeFileSync } from 'node:fs'
import { join } from 'node:path'
import { fileURLToPath } from 'node:url'

const root = fileURLToPath(new URL('../dist/types', import.meta.url))

function declarations(dir: string): string[] {
  const found: string[] = []
  for (const entry of readdirSync(dir)) {
    const path = join(dir, entry)
    if (statSync(path).isDirectory()) found.push(...declarations(path))
    else if (entry.endsWith('.d.ts')) found.push(path)
  }
  return found
}

function withExtensions(source: string): string {
  return source
    .replace(/(\bfrom\s+['"])(\.{1,2}\/[^'"]+)(['"])/g, (match, before, spec, after) =>
      spec.endsWith('.js') ? match : `${before}${spec}.js${after}`,
    )
    .replace(/(\bimport\(\s*['"])(\.{1,2}\/[^'"]+)(['"]\s*\))/g, (match, before, spec, after) =>
      spec.endsWith('.js') ? match : `${before}${spec}.js${after}`,
    )
}

let rewritten = 0
for (const file of declarations(root)) {
  const source = readFileSync(file, 'utf8')
  const next = withExtensions(source)
  if (next !== source) {
    writeFileSync(file, next)
    rewritten++
  }
}

console.log(`fix-dts: ${rewritten} declaration file(s) rewritten for NodeNext`)
