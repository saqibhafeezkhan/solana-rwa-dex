declare module 'buffer' {
  export const Buffer: {
    from(data: Iterable<number> | ArrayBufferLike): Uint8Array & { buffer: ArrayBufferLike }
  }
}
