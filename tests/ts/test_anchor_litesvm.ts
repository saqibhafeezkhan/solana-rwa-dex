import * as anchor from "@coral-xyz/anchor";
import { strict as assert } from "assert";

const provider = anchor.AnchorProvider.env();
anchor.setProvider(provider);

describe("solana_dex (LiteSVM TS smoke)", () => {
  it("connects to validator", async () => {
    const version = await provider.connection.getVersion();
    console.log("validator version:", version);
    assert.ok(version && Object.keys(version).length > 0, "got validator version");
  }).timeout(20000);
});
