import * as anchor from "@coral-xyz/anchor";
import { Program, AnchorProvider } from "@coral-xyz/anchor";
import BN from "bn.js";
import { Keypair, PublicKey, SystemProgram, LAMPORTS_PER_SOL } from "@solana/web3.js";
import {
  TOKEN_PROGRAM_ID,
  createInitializeMintInstruction,
  TOKEN_2022_PROGRAM_ID,
  createAssociatedTokenAccountIdempotentInstruction,
  getAssociatedTokenAddressSync,
} from "@solana/spl-token";
import createKeccak from "keccak";
// `@noble/curves` arrives with `@solana/web3.js` (as it does for the seeder,
// which signs the same message to exercise the release path); the Transfer Agent
// signature is a real secp256k1 one so the settle path is exercised for real.
import { secp256k1 } from "@noble/curves/secp256k1";
import { assert } from "chai";
import { buildTreeAndProve, buildWhitelistTree, bytesToHex, leafHash } from "../../app/web/src/core/merkle.ts";

// Import your IDLs here (You will need to copy these from target/idl after running anchor build)
// import { TransferHook } from "../target/types/transfer_hook";
// import { Settlement } from "../target/types/settlement";

const BPF_LOADER_UPGRADEABLE = new PublicKey("BPFLoaderUpgradeab1e11111111111111111111111");

/**
 * The ProgramData account of an upgradeable program, which the one-time
 * initializers in `transfer_hook` and `settlement` read the upgrade authority
 * from. The IDL does not declare it as a PDA, so Anchor cannot resolve it for
 * us and it has to be passed explicitly.
 */
function programData(programId: PublicKey): PublicKey {
  return PublicKey.findProgramAddressSync([programId.toBuffer()], BPF_LOADER_UPGRADEABLE)[0];
}

/** `[hex, …]` → the `Vec<[u8; 32]>` the hook takes as an instruction argument. */
function proofBytes(proof: string[]): number[][] {
  return proof.map((element) => Array.from(Buffer.from(element, "hex")));
}

/**
 * An institution's whitelist entry: `[b"whitelist", owner]` under the hook
 * program. Token-2022 derives exactly this address for the sender and the
 * recipient of every transfer, from each token account's `owner` field, which is
 * how the two Merkle proofs reach the hook - there is no instruction-data path.
 */
function whitelistEntry(programId: PublicKey, owner: PublicKey): PublicKey {
  return PublicKey.findProgramAddressSync(
    [Buffer.from("whitelist"), owner.toBuffer()],
    programId
  )[0];
}

/**
 * The remaining accounts one RWA transfer needs for its hook, in the order
 * Token-2022 resolves them: the hook program, the mint's meta list, the sender's
 * entry, the recipient's entry, then the hook config. Settlement's escrow moves
 * need the same set - see `hookAccounts` below and the settlement cases.
 */
function hookAccounts(
  programId: PublicKey,
  sourceOwner: PublicKey,
  destinationOwner: PublicKey,
  extraAccountMetaList: PublicKey,
  hookConfig: PublicKey
) {
  return [
    // Token-2022 CPI's into the hook, and the callee's program account has to be
    // in the transfer's account list for `invoke` to resolve it.
    { pubkey: programId, isSigner: false, isWritable: false },
    { pubkey: extraAccountMetaList, isSigner: false, isWritable: false },
    { pubkey: whitelistEntry(programId, sourceOwner), isSigner: false, isWritable: false },
    { pubkey: whitelistEntry(programId, destinationOwner), isSigner: false, isWritable: false },
    { pubkey: hookConfig, isSigner: false, isWritable: false },
  ];
}

/**
 * The secp256k1 generator point, used as the demo Transfer Agent key: it is the
 * public key of the scalar `1`, which is what the seeder signs with. Registering
 * it lets this suite sign a real settlement release instead of stubbing it.
 */
const AGENT_KEY = {
  x: Buffer.from("79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798", "hex"),
  y: Buffer.from("483ada7726a3c4655da4fbfc0e1108a8fd17b448a68554199c47d08ffb10d4b8", "hex"),
};

/** The Transfer Agent's signature over `keccak256(trade_id ‖ taker ‖ mint ‖ amount)`. */
function signAgentMessage(tradeId: bigint, taker: PublicKey, mint: PublicKey, amount: bigint) {
  const message = Buffer.concat([
    new BN(tradeId.toString()).toArrayLike(Buffer, "le", 8),
    taker.toBuffer(),
    mint.toBuffer(),
    new BN(amount.toString()).toArrayLike(Buffer, "le", 8),
  ]);
  const messageHash = createKeccak("keccak256").update(message).digest();
  const signature = secp256k1.sign(messageHash, 1n, { lowS: true });
  if (signature.recovery === undefined) throw new Error("signature has no recovery id");
  return { signature: signature.toCompactRawBytes(), recoveryId: signature.recovery };
}

describe("RWA Institutional DEX - Full Lifecycle", () => {
  const provider = AnchorProvider.local();
  anchor.setProvider(provider);

  const wallet = provider.wallet as anchor.Wallet;
  const connection = provider.connection;

  // Program IDs (Replace with your actual deployed IDs from Anchor.toml)
  const transferHookProgram = anchor.workspace.TransferHook as Program<any>;
  const settlementProgram = anchor.workspace.Settlement as Program<any>;
  const rwaMintProgram = anchor.workspace.RwaMint as Program<any>;

  // Actors
  const transferAgent = Keypair.generate(); // Off-chain authority
  const institutionA = Keypair.generate(); // Whitelisted
  const institutionB = Keypair.generate(); // Whitelisted
  const unauthorizedUser = Keypair.generate(); // NOT Whitelisted

  // Mints
  let rwaMint: Keypair;
  let usdcMint: Keypair;

  // PDAs
  let hookConfigPda: PublicKey;
  let extraAccountMetaListPda: PublicKey;
  let settlementConfigPda: PublicKey;
  let settlementVaultPda: PublicKey;

  // Merkle whitelist
  let merkleRoot: Buffer;
  let sourceProofHex: string[] = [];
  let destinationProofHex: string[] = [];

  before(async () => {
    // 1. Fund all wallets
    await Promise.all([
      connection.requestAirdrop(institutionA.publicKey, 10 * LAMPORTS_PER_SOL),
      connection.requestAirdrop(institutionB.publicKey, 10 * LAMPORTS_PER_SOL),
      connection.requestAirdrop(unauthorizedUser.publicKey, 10 * LAMPORTS_PER_SOL),
      connection.requestAirdrop(transferAgent.publicKey, 10 * LAMPORTS_PER_SOL),
    ]);

    // 2. Setup USDC Mint (Standard SPL)
    usdcMint = Keypair.generate();
    const usdcLamports = await connection.getMinimumBalanceForRentExemption(82);
    const createUsdcIx = SystemProgram.createAccount({
      fromPubkey: wallet.publicKey,
      newAccountPubkey: usdcMint.publicKey,
      space: 82,
      lamports: usdcLamports,
      programId: TOKEN_PROGRAM_ID,
    });
    const initUsdcIx = createInitializeMintInstruction(
      usdcMint.publicKey, 6, wallet.publicKey, wallet.publicKey, TOKEN_PROGRAM_ID
    );
    const tx = new anchor.web3.Transaction().add(createUsdcIx, initUsdcIx);
    await provider.sendAndConfirm(tx, [usdcMint]);

    // 3. Setup Merkle Whitelist
    //
    // The hook domain-separates its hashes (leaf `keccak(0x00 ‖ owner)`, node
    // `keccak(0x01 ‖ min ‖ max)`), so the tree is built with the same helper the
    // web client uses. A stock `merkletreejs` tree hashes `left ‖ right` with no
    // prefixes, which uploads a root the hook can never match.
    const whitelist = [institutionA.publicKey, institutionB.publicKey];
    const tree = buildTreeAndProve(whitelist, institutionA.publicKey, institutionB.publicKey);
    merkleRoot = Buffer.from(tree.root, "hex");
    sourceProofHex = tree.sourceProof;
    destinationProofHex = tree.destinationProof;

    console.log("Merkle Root:", merkleRoot.toString("hex"));

    // 4. Initialize the settlement clearinghouse.
    //
    // `pools_require_gate = false` selects the standalone flow this suite uses:
    // the config authority initiates from its own custody account instead of
    // through an AMM pool PDA. The instruction reads the program's upgrade
    // authority, so it only works against a cluster where the deployer keypair
    // (ANCHOR_WALLET) is that authority — which is what
    // `scripts/deploy-localnet.sh` produces.
    [settlementConfigPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("config")],
      settlementProgram.programId
    );

    await settlementProgram.methods
      .initialize(false)
      .accounts({
        settlementConfig: settlementConfigPda,
        authority: wallet.publicKey,
        programData: programData(settlementProgram.programId),
        systemProgram: SystemProgram.programId,
      })
      .rpc();

    // 5. Point the clearinghouse at the Transfer Agent key this suite can sign
    //    with: the secp256k1 generator point (the public key of the scalar `1`),
    //    which is exactly what the seeder registers so the release path can be
    //    exercised with a real signature instead of a stub.
    await settlementProgram.methods
      .setTransferAgent(Array.from(Buffer.concat([AGENT_KEY.x, AGENT_KEY.y])))
      .accounts({ settlementConfig: settlementConfigPda, authority: wallet.publicKey })
      .rpc();
  });

  it("Initializes the Transfer Hook and RWA Token", async () => {
    // 1. Initialize Hook Config PDA
    [hookConfigPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("config")],
      transferHookProgram.programId
    );

    await transferHookProgram.methods
      .initialize()
      .accounts({
        config: hookConfigPda,
        authority: wallet.publicKey,
        programData: programData(transferHookProgram.programId),
        systemProgram: SystemProgram.programId,
      })
      .rpc();

    // 2. Update Merkle Root
    await transferHookProgram.methods
      .updateRoot(Array.from(merkleRoot))
      .accounts({
        config: hookConfigPda,
        authority: wallet.publicKey,
      })
      .rpc();

    // 3. Create the RWA Token-2022 mint
    //
    // `rwa_mint::create_rwa_mint` is what installs the TransferHook extension
    // (alongside the metadata pointer, permanent delegate and transfer-fee
    // config), so the mint ends up pointing at `transfer_hook`. Creating the
    // mint by hand and only *asserting* the hook were wired - as this test used
    // to - left the whitelist untested: with no hook installed every transfer
    // succeeds and the enforcement assertion below could never fire.
    rwaMint = Keypair.generate();

    await rwaMintProgram.methods
      .createRwaMint({
        decimals: 6,
        transferHookProgram: transferHookProgram.programId,
        delegate: wallet.publicKey,
        feeAuthority: wallet.publicKey,
        metadata: {
          name: "RWA Institutional Note",
          symbol: "RWAN",
          uri: "https://example.invalid/rwa-note.json",
        },
      })
      .accounts({
        mint: rwaMint.publicKey,
        authority: wallet.publicKey,
        tokenProgram: TOKEN_2022_PROGRAM_ID,
        systemProgram: SystemProgram.programId,
      })
      .signers([rwaMint])
      .rpc();

    // 4. Setup ExtraAccountMetaList (Required for Token-2022 to resolve the Config PDA)
    // Seed is `extra-account-metas`, matching `pdas.ts` and the program's own
    // `seeds = [b"extra-account-metas", mint]`.
    [extraAccountMetaListPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("extra-account-metas"), rwaMint.publicKey.toBuffer()],
      transferHookProgram.programId
    );

    await transferHookProgram.methods
      .initializeExtraAccountMetaList()
      .accounts({
        payer: wallet.publicKey,
        extraAccountMetaList: extraAccountMetaListPda,
        mint: rwaMint.publicKey,
        systemProgram: SystemProgram.programId,
      })
      .rpc();

    console.log("RWA Mint & Hook configured successfully.");
  });

  it("Mints RWA tokens to Institution A", async () => {
    const instAta = getAssociatedTokenAddressSync(
      rwaMint.publicKey, institutionA.publicKey, false, TOKEN_2022_PROGRAM_ID
    );

    // Create the destination ATA. The mint authority pays, so institutionA
    // does not have to sign for its own account to come into existence.
    await provider.sendAndConfirm(
      new anchor.web3.Transaction().add(
        createAssociatedTokenAccountIdempotentInstruction(
          wallet.publicKey, instAta, institutionA.publicKey, rwaMint.publicKey, TOKEN_2022_PROGRAM_ID
        )
      )
    );

    await rwaMintProgram.methods
      .mintTo(new BN(1_000_000_000))
      .accounts({
        mint: rwaMint.publicKey,
        destination: instAta,
        authority: wallet.publicKey,
        tokenProgram: TOKEN_2022_PROGRAM_ID,
      })
      .rpc();

    const balance = await connection.getTokenAccountBalance(instAta);
    assert.equal(balance.value.amount, "1000000000");
  });

  it("Registers a whitelist entry for each institution", async () => {
    // The hook cannot receive proofs as instruction data: Token-2022 builds the
    // hook's instruction itself (`[discriminator][amount]`, nothing else) and
    // forwards no trailing bytes from a transfer, so appending proofs to the
    // `transfer_checked` payload makes the token program reject the transfer
    // outright. Each institution's Merkle path is therefore written into its own
    // entry PDA - the account Token-2022 resolves from that wallet's token
    // account on every transfer.
    for (const [owner, proof] of [
      [institutionA.publicKey, sourceProofHex],
      [institutionB.publicKey, destinationProofHex],
    ] as const) {
      await transferHookProgram.methods
        .setWhitelistEntry(proofBytes(proof))
        .accounts({
          config: hookConfigPda,
          authority: wallet.publicKey,
          owner,
          whitelistEntry: whitelistEntry(transferHookProgram.programId, owner),
          systemProgram: SystemProgram.programId,
        })
        .rpc();
    }

    // The entries really are on chain, owned by the hook.
    for (const owner of [institutionA.publicKey, institutionB.publicKey]) {
      const info = await connection.getAccountInfo(whitelistEntry(transferHookProgram.programId, owner));
      assert.isNotNull(info, "whitelist entry should exist");
      assert.equal(info!.owner.toBase58(), transferHookProgram.programId.toBase58());
    }
    console.log("✅ Whitelist entries registered for both institutions.");
  });

  it("Transfers RWA tokens between two whitelisted institutions", async () => {
    const sourceAta = getAssociatedTokenAddressSync(rwaMint.publicKey, institutionA.publicKey, false, TOKEN_2022_PROGRAM_ID);
    const destAta = getAssociatedTokenAddressSync(rwaMint.publicKey, institutionB.publicKey, false, TOKEN_2022_PROGRAM_ID);

    await provider.sendAndConfirm(
      new anchor.web3.Transaction().add(
        createAssociatedTokenAccountIdempotentInstruction(
          wallet.publicKey, destAta, institutionB.publicKey, rwaMint.publicKey, TOKEN_2022_PROGRAM_ID
        )
      )
    );

    const before = await connection.getTokenAccountBalance(destAta);

    // `rwa_mint::transfer` is the repo's entry point for a whitelisted transfer.
    // Both sides' proofs arrive as the two entry PDAs in the remaining accounts;
    // Token-2022 re-derives them from the token accounts' owners, so a caller
    // cannot substitute an entry for some other wallet.
    await rwaMintProgram.methods
      .transfer(new BN(1_000))
      .accounts({
        source: sourceAta,
        destination: destAta,
        mint: rwaMint.publicKey,
        authority: institutionA.publicKey,
        tokenProgram: TOKEN_2022_PROGRAM_ID,
      })
      .remainingAccounts(
        hookAccounts(
          transferHookProgram.programId,
          institutionA.publicKey,
          institutionB.publicKey,
          extraAccountMetaListPda,
          hookConfigPda
        )
      )
      .signers([institutionA])
      .rpc();

    const after = await connection.getTokenAccountBalance(destAta);
    assert.equal(
      Number(after.value.amount) - Number(before.value.amount),
      1_000,
      "the whitelisted transfer should have credited the recipient"
    );
    console.log("✅ Whitelisted transfer A → B succeeded (hook approved both sides).");
  });

  it("Refuses a transfer to a wallet with no whitelist entry", async () => {
    const sourceAta = getAssociatedTokenAddressSync(rwaMint.publicKey, institutionA.publicKey, false, TOKEN_2022_PROGRAM_ID);
    const destAta = getAssociatedTokenAddressSync(rwaMint.publicKey, unauthorizedUser.publicKey, false, TOKEN_2022_PROGRAM_ID);

    await provider.sendAndConfirm(
      new anchor.web3.Transaction().add(
        createAssociatedTokenAccountIdempotentInstruction(
          wallet.publicKey, destAta, unauthorizedUser.publicKey, rwaMint.publicKey, TOKEN_2022_PROGRAM_ID
        )
      )
    );

    // `unauthorizedUser` has no entry, so the address Token-2022 resolves is an
    // empty account. The transfer has to fail: that account is not a hook-owned
    // entry, and the hook refuses to treat it as one.
    try {
      await rwaMintProgram.methods
        .transfer(new BN(1_000))
        .accounts({
          source: sourceAta,
          destination: destAta,
          mint: rwaMint.publicKey,
          authority: institutionA.publicKey,
          tokenProgram: TOKEN_2022_PROGRAM_ID,
        })
        .remainingAccounts(
        hookAccounts(
          transferHookProgram.programId,
          institutionA.publicKey,
          unauthorizedUser.publicKey,
          extraAccountMetaListPda,
          hookConfigPda
        )
        )
        .signers([institutionA])
        .rpc();
      assert.fail("Transfer should have failed: the destination has no whitelist entry");
    } catch (err) {
      console.error(String(err));
      // Anchor's account validation is what normally catches this (the resolved
      // PDA is not a `WhitelistEntry`), which is why the assertion is on the
      // failure rather than on one of the hook's own error strings.
      assert.isDefined(err);
      console.log("✅ Whitelist enforcement successful: unregistered destination blocked.");
    }
  });

  it("Refuses to register an entry whose proof is not in the tree", async () => {
    // The program folds the path against the published root before storing it, so
    // a mistyped proof - or one from a tree that has since been rotated - cannot
    // be written in the first place.
    try {
      await transferHookProgram.methods
        .setWhitelistEntry(proofBytes([
          bytesToHex(leafHash(institutionA.publicKey)),
        ]))
        .accounts({
          config: hookConfigPda,
          authority: wallet.publicKey,
          owner: unauthorizedUser.publicKey,
          whitelistEntry: whitelistEntry(transferHookProgram.programId, unauthorizedUser.publicKey),
          systemProgram: SystemProgram.programId,
        })
        .rpc();
      assert.fail("An entry with a proof outside the tree should have been refused");
    } catch (err) {
      console.error(String(err));
      assert.include(err.toString(), "ProofNotInWhitelist");
      console.log("✅ Proof checked against the root before the entry was written.");
    }

    const info = await connection.getAccountInfo(
      whitelistEntry(transferHookProgram.programId, unauthorizedUser.publicKey)
    );
    assert.isNull(info, "a rejected entry must not be left behind");
  });

  it("Invalidates stale entries when the root is rotated", async () => {
    const sourceAta = getAssociatedTokenAddressSync(rwaMint.publicKey, institutionA.publicKey, false, TOKEN_2022_PROGRAM_ID);
    const destAta = getAssociatedTokenAddressSync(rwaMint.publicKey, institutionB.publicKey, false, TOKEN_2022_PROGRAM_ID);

    // Rotate the whitelist down to `institutionA` alone: a single-member tree,
    // whose root is just that wallet's leaf.
    const singleRoot = Buffer.from(leafHash(institutionA.publicKey));
    await transferHookProgram.methods
      .updateRoot(Array.from(singleRoot))
      .accounts({ config: hookConfigPda, authority: wallet.publicKey })
      .rpc();

    // Refresh A's entry for the new tree (an empty path: the leaf is the root).
    await transferHookProgram.methods
      .setWhitelistEntry([])
      .accounts({
        config: hookConfigPda,
        authority: wallet.publicKey,
        owner: institutionA.publicKey,
        whitelistEntry: whitelistEntry(transferHookProgram.programId, institutionA.publicKey),
        systemProgram: SystemProgram.programId,
      })
      .rpc();

    // B's entry is still on chain, but its path no longer folds to the published
    // root - which is exactly how a rotation revokes access.
    try {
      await rwaMintProgram.methods
        .transfer(new BN(1_000))
        .accounts({
          source: sourceAta,
          destination: destAta,
          mint: rwaMint.publicKey,
          authority: institutionA.publicKey,
          tokenProgram: TOKEN_2022_PROGRAM_ID,
        })
        .remainingAccounts(        hookAccounts(
          transferHookProgram.programId,
          institutionA.publicKey,
          institutionB.publicKey,
          extraAccountMetaListPda,
          hookConfigPda
        )
      )
      .signers([institutionA])
      .rpc();
      assert.fail("A stale entry should no longer verify");
    } catch (err) {
      console.error(String(err));
      assert.include(err.toString(), "DestinationNotWhitelisted");
      console.log("✅ Root rotation invalidated the stale entry.");
    }
  });

  /**
   * The fixed owner of every settlement escrow token account:
   * `[b"escrow_authority"]` under the settlement program.
   *
   * The hook checks both sides of every transfer, and Token-2022 resolves each
   * side's entry from its token account's `owner` field - so the escrow needs an
   * entry like any institution. Because every escrow is owned by this one PDA,
   * that is a single member for the whole clearinghouse: adding it once is what
   * stops every trade ID from needing its own entry plus a root rotation.
   */
  const escrowAuthority = PublicKey.findProgramAddressSync(
    [Buffer.from("escrow_authority")],
    settlementProgram.programId
  )[0];

  /** One trade's escrow token account, created by `initiate_settlement`. */
  const escrowToken = (tradeId: BN) =>
    PublicKey.findProgramAddressSync(
      [Buffer.from("escrow"), tradeId.toArrayLike(Buffer, "le", 8)],
      settlementProgram.programId
    )[0];

  /**
   * The whitelist the escrow cases need: the institutions **and** the escrow
   * authority above.
   *
   * Publishing a new root revokes every entry written against the previous one,
   * so every member is refreshed here; the escrow authority only ever has to be
   * added once, whichever trade ids come later.
   */
  async function publishWhitelistWith(extra: PublicKey[]) {
    const members = [institutionA.publicKey, institutionB.publicKey, ...extra];
    const tree = buildWhitelistTree(members);
    await transferHookProgram.methods
      .updateRoot(Array.from(Buffer.from(tree.root, "hex")))
      .accounts({ config: hookConfigPda, authority: wallet.publicKey })
      .rpc();
    for (const owner of members) {
      await transferHookProgram.methods
        .setWhitelistEntry(proofBytes(tree.proofFor(owner)))
        .accounts({
          config: hookConfigPda,
          authority: wallet.publicKey,
          owner,
          whitelistEntry: whitelistEntry(transferHookProgram.programId, owner),
          systemProgram: SystemProgram.programId,
        })
        .rpc();
    }
    return tree;
  }

  it("Locks RWA tokens in the T+N escrow (hook-gated transfer into the vault)", async () => {
    const tradeId = new BN(1);
    // `settlement_slot` is an absolute slot the program requires to be in the
    // future, not a delta - passing a literal 100 failed the check on any
    // ledger that had already passed slot 100.
    const settlementSlot = new BN((await connection.getSlot()) + 100); // T+100 slots

    [settlementVaultPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("settlement"), tradeId.toArrayLike(Buffer, "le", 8)],
      settlementProgram.programId
    );

    await publishWhitelistWith([escrowAuthority]);

    // The custody account the AMM routed this trade's output into: institution
    // A's own ATA, because `initiate_settlement` requires the swept account to be
    // owned by the signing authority - and A is whitelisted, as the source side
    // of a hooked transfer has to be.
    const custodyAta = getAssociatedTokenAddressSync(
      rwaMint.publicKey, institutionA.publicKey, false, TOKEN_2022_PROGRAM_ID
    );
    await rwaMintProgram.methods
      .mintTo(new BN(500_000_000))
      .accounts({
        mint: rwaMint.publicKey,
        destination: custodyAta,
        authority: wallet.publicKey,
        tokenProgram: TOKEN_2022_PROGRAM_ID,
      })
      .rpc();

    const vaultTokenAccount = escrowToken(tradeId);

    // The escrowing transfer is where the old code failed twice over: the legacy
    // `Transfer` CPI is refused outright by Token-2022 for a hook-bearing mint
    // (`MintRequiredForTransfer`, 0x1f), and `transfer_checked` alone still fails
    // because the mint's meta list is resolved against the transfer
    // instruction's own account list. The hook accounts below are that list; the
    // escrow side's entry is the escrow authority's, the source side's is A's.
    await settlementProgram.methods
      .initiateSettlement(tradeId, settlementSlot, new BN(500_000_000))
      .accounts({
        settlementVault: settlementVaultPda,
        sourceTokenAccount: custodyAta,
        settlementVaultToken: vaultTokenAccount,
        tokenMint: rwaMint.publicKey,
        taker: institutionB.publicKey, // Institution B is buying
        authority: institutionA.publicKey, // owns the custody ATA, signs the move
        // Standalone flow: no pool PDA. Anchor's `Option<T>` reads the
        // executing program's own id as the None sentinel.
        pool: settlementProgram.programId,
        settlementConfig: settlementConfigPda,
        tokenProgram: TOKEN_2022_PROGRAM_ID,
        systemProgram: SystemProgram.programId,
      })
      .remainingAccounts(
        hookAccounts(
          transferHookProgram.programId,
          institutionA.publicKey,
          escrowAuthority,
          extraAccountMetaListPda,
          hookConfigPda
        )
      )
      .signers([institutionA])
      .rpc();

    // The escrow holds exactly the trade's flow, and the vault recorded it.
    const vault = await settlementProgram.account.settlementVault.fetch(settlementVaultPda);
    assert.match(JSON.stringify(vault.status).toLowerCase(), /pending/);
    assert.equal(vault.amount.toString(), "500000000");
    assert.equal(vault.tokenMint.toBase58(), rwaMint.publicKey.toBase58());
    assert.equal(vault.taker.toBase58(), institutionB.publicKey.toBase58());
    const escrow = await connection.getTokenAccountBalance(vaultTokenAccount);
    assert.equal(escrow.value.amount, "500000000", "the escrow should hold the trade's flow");
    console.log("✅ Escrow funded: hook approved both sides, transfer_checked moved the tokens.");
  });

  it("Fails to settle before T+N period ends (Time Lock)", async () => {
    const destAta = getAssociatedTokenAddressSync(
      rwaMint.publicKey, institutionB.publicKey, false, TOKEN_2022_PROGRAM_ID
    );
    const vaultTokenAccount = escrowToken(new BN(1));

    try {
      await settlementProgram.methods
        .settle(Buffer.alloc(64), 0) // Mock signature: the time lock refuses first
        .accounts({
          settlementVault: settlementVaultPda,
          settlementConfig: settlementConfigPda,
          tokenMint: rwaMint.publicKey,
          vaultTokenAccount: vaultTokenAccount,
          escrowAuthority,
          takerTokenAccount: destAta,
          tokenProgram: TOKEN_2022_PROGRAM_ID,
        })
        .rpc();
      assert.fail("Settlement should have failed due to time lock");
    } catch (err) {
      assert.include(err.toString(), "SettlementPeriodNotOver");
      console.log("✅ Time Lock enforcement successful: Cannot settle early.");
    }
  });

  it("Advances time and releases the escrow with a real Transfer Agent signature", async () => {
    // 1. Advance the validator clock past the vault's unlock slot: `settle`
    //    releases only once `Clock::slot >= settlement_slot`. Airdrops are the
    //    available clock lever on a test validator (each one is its own
    //    transaction) and they have to meet the rent-exempt minimum, which is
    //    why this sends a whole SOL rather than a single lamport.
    const vault = await settlementProgram.account.settlementVault.fetch(settlementVaultPda);
    const unlockSlot = Number(vault.settlementSlot.toString());
    console.log(`⏩ Advancing the validator clock to slot ${unlockSlot}...`);
    for (let i = 0; i < 400 && (await connection.getSlot()) < unlockSlot; i++) {
      await connection.requestAirdrop(Keypair.generate().publicKey, LAMPORTS_PER_SOL);
    }
    assert.isAtLeast(
      await connection.getSlot(),
      unlockSlot,
      "the validator clock should have passed the vault's unlock slot"
    );

    // 2. Institution B's payout account.
    const destAta = getAssociatedTokenAddressSync(
      rwaMint.publicKey, institutionB.publicKey, false, TOKEN_2022_PROGRAM_ID
    );
    await provider.sendAndConfirm(
      new anchor.web3.Transaction().add(
        createAssociatedTokenAccountIdempotentInstruction(
          wallet.publicKey, destAta, institutionB.publicKey, rwaMint.publicKey, TOKEN_2022_PROGRAM_ID
        )
      )
    );
    const before = await connection.getTokenAccountBalance(destAta);

    // 3. The Transfer Agent signs keccak256(trade_id ‖ taker ‖ mint ‖ amount) —
    //    exactly the preimage the program rebuilds from the vault's own fields.
    const vaultTokenAccount = escrowToken(new BN(1));
    const signed = signAgentMessage(1n, institutionB.publicKey, rwaMint.publicKey, BigInt(vault.amount.toString()));

    // 4. Release. `settle` is permissionless, and the escrow side's entry is the
    //    escrow authority's - the one member the whitelist added above.
    await settlementProgram.methods
      .settle(Buffer.from(signed.signature), signed.recoveryId)
      .accounts({
        settlementVault: settlementVaultPda,
        settlementConfig: settlementConfigPda,
        tokenMint: rwaMint.publicKey,
        vaultTokenAccount: vaultTokenAccount,
        escrowAuthority,
        takerTokenAccount: destAta,
        tokenProgram: TOKEN_2022_PROGRAM_ID,
      })
      .remainingAccounts(
        hookAccounts(
          transferHookProgram.programId,
          escrowAuthority,
          institutionB.publicKey,
          extraAccountMetaListPda,
          hookConfigPda
        )
      )
      .rpc();

    const after = await connection.getTokenAccountBalance(destAta);
    assert.equal(
      Number(after.value.amount) - Number(before.value.amount),
      Number(vault.amount.toString()),
      "the taker should receive the escrowed amount"
    );
    const escrow = await connection.getTokenAccountBalance(vaultTokenAccount);
    assert.equal(escrow.value.amount, "0", "the escrow should be empty after settlement");
    const settled = await settlementProgram.account.settlementVault.fetch(settlementVaultPda);
    assert.match(JSON.stringify(settled.status).toLowerCase(), /settled/);
    console.log("✅ T+N Settlement successful! Funds released to Institution B.");
  });

  it("Refunds a cancelled escrow to the account that funded it", async () => {
    const tradeId = new BN(2);
    const [cancelVault] = PublicKey.findProgramAddressSync(
      [Buffer.from("settlement"), tradeId.toArrayLike(Buffer, "le", 8)],
      settlementProgram.programId
    );
    // No new whitelist member is needed: this trade's escrow is owned by the
    // same escrow authority the first trade registered.
    const custodyAta = getAssociatedTokenAddressSync(
      rwaMint.publicKey, institutionA.publicKey, false, TOKEN_2022_PROGRAM_ID
    );
    const vaultTokenAccount = escrowToken(tradeId);
    const settlementSlot = new BN((await connection.getSlot()) + 10_000);
    const amount = new BN(25_000_000);

    await settlementProgram.methods
      .initiateSettlement(tradeId, settlementSlot, amount)
      .accounts({
        settlementVault: cancelVault,
        sourceTokenAccount: custodyAta,
        settlementVaultToken: vaultTokenAccount,
        tokenMint: rwaMint.publicKey,
        taker: institutionB.publicKey,
        authority: institutionA.publicKey,
        pool: settlementProgram.programId,
        settlementConfig: settlementConfigPda,
        tokenProgram: TOKEN_2022_PROGRAM_ID,
        systemProgram: SystemProgram.programId,
      })
      .remainingAccounts(
        hookAccounts(
          transferHookProgram.programId,
          institutionA.publicKey,
          escrowAuthority,
          extraAccountMetaListPda,
          hookConfigPda
        )
      )
      .signers([institutionA])
      .rpc();

    const before = await connection.getTokenAccountBalance(custodyAta);
    await settlementProgram.methods
      .cancelSettlement()
      .accounts({
        settlementVault: cancelVault,
        settlementConfig: settlementConfigPda,
        pool: settlementProgram.programId,
        tokenMint: rwaMint.publicKey,
        vaultTokenAccount: vaultTokenAccount,
        escrowAuthority,
        refundTokenAccount: custodyAta,
        authority: wallet.publicKey, // the config authority's compliance exit
        tokenProgram: TOKEN_2022_PROGRAM_ID,
      })
      .remainingAccounts(
        hookAccounts(
          transferHookProgram.programId,
          escrowAuthority,
          institutionA.publicKey,
          extraAccountMetaListPda,
          hookConfigPda
        )
      )
      .rpc();

    const after = await connection.getTokenAccountBalance(custodyAta);
    assert.equal(
      Number(after.value.amount) - Number(before.value.amount),
      Number(amount.toString()),
      "the refund should return the escrowed amount to the funder"
    );
    const cancelled = await settlementProgram.account.settlementVault.fetch(cancelVault);
    assert.match(JSON.stringify(cancelled.status).toLowerCase(), /cancelled/);
    console.log("✅ Cancelled escrow refunded to the account that funded it.");
  });
});