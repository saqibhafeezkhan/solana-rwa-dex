// web3.js v1 depends on the `buffer` npm package and expects `Buffer` to exist
// at runtime; the polyfill import in main.tsx provides it in the browser.
declare const Buffer: {
  from(data: Iterable<number> | ArrayBufferLike): {
    buffer: ArrayBufferLike
    byteOffset: number
    byteLength: number
  } & Uint8Array
} & Function
