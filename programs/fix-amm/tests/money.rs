use anchor_lang::{AccountDeserialize, AnchorDeserialize, Discriminator, InstructionData, ToAccountMetas};
use anchor_lang::solana_program::program_pack::Pack;
use base64::{engine::general_purpose::STANDARD, Engine};
use fix_amm::{accounts, instruction, Config, Pool, PoolCreated, Position, Protocol, Receipt, Terms, ID, LAUNCHPAD_ID, PROTOCOL_SHARDS};
use litesvm::{types::TransactionMetadata, LiteSVM};
use serde_json::{json, Value};
use solana_sdk::{account::Account, bpf_loader_upgradeable::{self, UpgradeableLoaderState}, clock::Clock, compute_budget::ComputeBudgetInstruction, instruction::Instruction, pubkey::Pubkey, signature::{Keypair, Signer}, system_instruction, transaction::Transaction};
use spl_token_2022::extension::{BaseStateWithExtensions, ExtensionType, StateWithExtensions};

/// The launch terms: 1% of the SOL side, 40% of it to the protocol and 60% kept by liquidity providers.
const TERMS: Terms = Terms { fee_bps: 100, protocol_share_bps: 4_000 };
const SOL: u64 = 1_000_000_000;
const TOKEN: u64 = 1_000_000; // six decimals

fn pda(seeds: &[&[u8]]) -> Pubkey { Pubkey::find_program_address(seeds, &ID).0 }
fn event_authority() -> Pubkey { pda(&[b"__event_authority"]) }
fn shard_of(pool: &Pubkey) -> u8 { pool.to_bytes()[0] % PROTOCOL_SHARDS }
fn ix(data: impl InstructionData, accs: impl ToAccountMetas) -> Instruction { Instruction { program_id: ID, data: data.data(), accounts: accs.to_account_metas(None) } }

fn events(meta: &TransactionMetadata) -> Vec<Vec<u8>> {
    meta.inner_instructions.iter().flatten().filter_map(|inner| inner.instruction.data.strip_prefix(anchor_lang::event::EVENT_IX_TAG_LE)).map(<[u8]>::to_vec).collect()
}
fn receipts(meta: &TransactionMetadata) -> Vec<Receipt> {
    events(meta).iter().filter(|b| b.starts_with(Receipt::DISCRIMINATOR)).map(|b| Receipt::deserialize(&mut &b[8..]).unwrap()).collect()
}
fn receipt(meta: &TransactionMetadata) -> Receipt { let mut all = receipts(meta); assert_eq!(all.len(), 1, "{:?}", meta.logs); all.remove(0) }
fn receipt_json(r: &Receipt) -> Value {
    json!({"action":r.action,"gross":r.gross.to_string(),"protocol":r.protocol_fee.to_string(),"lp":r.lp_fee.to_string(),"net":r.net.to_string(),"tokens":r.tokens.to_string(),
        "shares":r.shares.to_string(),"totalShares":r.total_shares.to_string(),"solReserve":r.sol_reserve.to_string(),"tokenReserve":r.token_reserve.to_string(),
        "priceBefore":r.price_before.to_string(),"priceAfter":r.price_after.to_string(),"priceImpactBps":r.price_impact_bps.to_string()})
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind { Spl, Token2022Metadata, Token2022(ExtensionType) }

#[derive(Clone, Copy)]
struct Asset { mint: Pubkey, program: Pubkey }
impl Asset {
    fn pool(&self) -> Pubkey { pda(&[b"pool", self.mint.as_ref()]) }
    fn vault(&self) -> Pubkey { pda(&[b"vault", self.pool().as_ref()]) }
    fn position(&self, owner: &Pubkey) -> Pubkey { pda(&[b"position", self.pool().as_ref(), owner.as_ref()]) }
    fn launchpad_market(&self) -> Pubkey { Pubkey::find_program_address(&[b"market", self.mint.as_ref()], &LAUNCHPAD_ID).0 }
    fn protocol(&self) -> Pubkey { pda(&[b"protocol", &[shard_of(&self.pool())]]) }
}

struct Harness { svm: LiteSVM, payer: Keypair, admin: Keypair, config: Pubkey, recorded: Vec<Value> }
impl Harness {
    fn new() -> Self { let mut h = Self::uninitialized(); h.initialize(); h }
    fn uninitialized() -> Self {
        let mut svm = LiteSVM::new();
        let artifact = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/deploy/fix_amm.so");
        assert!(artifact.exists(), "Build SBF first: cargo-build-sbf --manifest-path programs/fix-amm/Cargo.toml --tools-version v1.52 --arch v0");
        let elf = std::fs::read(artifact).unwrap();
        let payer = Keypair::new(); let admin = Keypair::new();
        svm.airdrop(&payer.pubkey(), 1_000 * SOL).unwrap(); svm.airdrop(&admin.pubkey(), 10 * SOL).unwrap();
        let program_data = Pubkey::find_program_address(&[ID.as_ref()], &bpf_loader_upgradeable::ID).0;
        let mut bytes = bincode::serialize(&UpgradeableLoaderState::ProgramData { slot: 0, upgrade_authority_address: Some(admin.pubkey()) }).unwrap();
        bytes.resize(UpgradeableLoaderState::size_of_programdata_metadata(), 0); bytes.extend_from_slice(&elf);
        let rent: solana_sdk::rent::Rent = svm.get_sysvar();
        svm.set_account(program_data, Account { lamports: rent.minimum_balance(bytes.len()), data: bytes, owner: bpf_loader_upgradeable::ID, executable: false, rent_epoch: 0 }).unwrap();
        let data = bincode::serialize(&UpgradeableLoaderState::Program { programdata_address: program_data }).unwrap();
        svm.set_account(ID, Account { lamports: rent.minimum_balance(data.len()), data, owner: bpf_loader_upgradeable::ID, executable: true, rent_epoch: 0 }).unwrap();
        let mut h = Self { svm, payer, admin, config: pda(&[b"config"]), recorded: vec![] };
        h.time(1_000); h
    }
    fn init_ix(&self, admin: Pubkey) -> Instruction {
        let program_data = Pubkey::find_program_address(&[ID.as_ref()], &bpf_loader_upgradeable::ID).0;
        ix(instruction::InitializeConfig { terms: TERMS }, accounts::InitializeConfig { admin, program: ID, program_data, config: self.config, system_program: solana_sdk::system_program::ID })
    }
    fn initialize(&mut self) {
        let admin = self.admin.insecure_clone();
        self.send(vec![self.init_ix(admin.pubkey())], &[&admin]).unwrap();
        for chunk in (0..PROTOCOL_SHARDS).collect::<Vec<_>>().chunks(16) { let ixs = chunk.iter().map(|s| self.shard_ix(*s)).collect(); self.send(ixs, &[]).unwrap(); }
    }
    fn shard_ix(&self, shard: u8) -> Instruction {
        ix(instruction::CreateProtocolShard { shard }, accounts::CreateProtocolShard { payer: self.payer.pubkey(), protocol: pda(&[b"protocol", &[shard]]), system_program: solana_sdk::system_program::ID })
    }
    fn time(&mut self, t: i64) { let mut c: Clock = self.svm.get_sysvar(); c.unix_timestamp = t; self.svm.set_sysvar(&c); }
    fn slot(&self) -> u64 { self.svm.get_sysvar::<Clock>().slot }
    fn advance(&mut self) { let mut c: Clock = self.svm.get_sysvar(); c.slot += 1; c.unix_timestamp += 1; self.svm.set_sysvar(&c); }
    fn send(&mut self, ixs: Vec<Instruction>, signers: &[&Keypair]) -> Result<TransactionMetadata, String> { self.advance(); self.send_same_slot(ixs, signers) }
    fn send_same_slot(&mut self, mut ixs: Vec<Instruction>, signers: &[&Keypair]) -> Result<TransactionMetadata, String> {
        self.svm.expire_blockhash();
        ixs.insert(0, ComputeBudgetInstruction::set_compute_unit_limit(1_400_000));
        let mut all: Vec<&Keypair> = vec![&self.payer]; all.extend_from_slice(signers);
        let tx = Transaction::new_signed_with_payer(&ixs, Some(&self.payer.pubkey()), &all, self.svm.latest_blockhash());
        let bytes = STANDARD.encode(bincode::serialize(&tx).unwrap());
        let meta = self.svm.send_transaction(tx).map_err(|e| format!("{:?}\n{:?}", e.err, e.meta.logs))?;
        let clock: Clock = self.svm.get_sysvar();
        let inner: Vec<Value> = meta.inner_instructions.iter().enumerate().filter(|(_, list)| !list.is_empty()).map(|(index, list)| json!({
            "index": index,
            "instructions": list.iter().map(|entry| json!({"programIdIndex": entry.instruction.program_id_index, "accounts": entry.instruction.accounts, "dataBase64": STANDARD.encode(&entry.instruction.data), "stackHeight": entry.stack_height})).collect::<Vec<_>>(),
        })).collect();
        self.recorded.push(json!({"signature": meta.signature.to_string(), "slot": clock.slot, "unixTimestamp": clock.unix_timestamp, "transactionBase64": bytes, "logs": meta.logs, "innerInstructions": inner}));
        Ok(meta)
    }
    fn wallet(&mut self, sol: u64) -> Keypair { let w = Keypair::new(); self.svm.airdrop(&w.pubkey(), sol).unwrap(); w }
    fn balance(&self, key: Pubkey) -> u64 { self.svm.get_balance(&key).unwrap_or(0) }
    fn rent(&self, key: Pubkey) -> u64 { let r: solana_sdk::rent::Rent = self.svm.get_sysvar(); r.minimum_balance(self.svm.get_account(&key).unwrap().data.len()) }
    fn decode<T: AccountDeserialize>(&self, key: Pubkey) -> T { T::try_deserialize(&mut self.svm.get_account(&key).unwrap().data.as_slice()).unwrap() }
    fn pool(&self, a: Asset) -> Pool { self.decode(a.pool()) }
    fn tokens(&self, account: Pubkey) -> u64 { StateWithExtensions::<spl_token_2022::state::Account>::unpack(&self.svm.get_account(&account).unwrap().data).unwrap().base.amount }

    /// A six-decimal mint with the payer as its authority.
    fn mint(&mut self, kind: Kind) -> Asset {
        let mint = Keypair::new(); let payer = self.payer.pubkey(); let rent: solana_sdk::rent::Rent = self.svm.get_sysvar();
        if kind == Kind::Spl {
            let ixs = vec![system_instruction::create_account(&payer, &mint.pubkey(), rent.minimum_balance(82), 82, &spl_token::ID), spl_token::instruction::initialize_mint2(&spl_token::ID, &mint.pubkey(), &payer, None, 6).unwrap()];
            self.send(ixs, &[&mint]).unwrap();
            return Asset { mint: mint.pubkey(), program: spl_token::ID };
        }
        let program = spl_token_2022::ID; let key = mint.pubkey();
        let extension = match kind { Kind::Token2022(e) => e, _ => ExtensionType::MetadataPointer };
        let len = ExtensionType::try_calculate_account_len::<spl_token_2022::state::Mint>(&[extension]).unwrap();
        // Metadata grows the mint after initialization: fund its final size up front, as a launch does.
        let funded = if kind == Kind::Token2022Metadata { rent.minimum_balance(len + 400) } else { rent.minimum_balance(len) };
        let mut ixs = vec![system_instruction::create_account(&payer, &key, funded, len as u64, &program)];
        ixs.push(match extension {
            ExtensionType::MetadataPointer => spl_token_2022::extension::metadata_pointer::instruction::initialize(&program, &key, None, Some(key)).unwrap(),
            ExtensionType::TransferFeeConfig => spl_token_2022::extension::transfer_fee::instruction::initialize_transfer_fee_config(&program, &key, None, None, 100, 1_000_000).unwrap(),
            ExtensionType::PermanentDelegate => spl_token_2022::instruction::initialize_permanent_delegate(&program, &key, &payer).unwrap(),
            ExtensionType::MintCloseAuthority => spl_token_2022::instruction::initialize_mint_close_authority(&program, &key, Some(&payer)).unwrap(),
            ExtensionType::InterestBearingConfig => spl_token_2022::extension::interest_bearing_mint::instruction::initialize(&program, &key, None, 500).unwrap(),
            other => panic!("unsupported fixture extension {other:?}"),
        });
        ixs.push(spl_token_2022::instruction::initialize_mint2(&program, &key, &payer, None, 6).unwrap());
        if kind == Kind::Token2022Metadata {
            ixs.push(spl_token_metadata_interface::instruction::initialize(&program, &key, &payer, &key, &payer, "Outside Coin".into(), "OUT".into(), "https://example.invalid/out.json".into()));
        }
        self.send(ixs, &[&mint]).unwrap();
        Asset { mint: key, program }
    }
    /// A token account for `owner`, funded with `amount` atoms.
    fn holder(&mut self, a: Asset, owner: Pubkey, amount: u64) -> Pubkey {
        let account = Keypair::new(); let rent: solana_sdk::rent::Rent = self.svm.get_sysvar(); let payer = self.payer.pubkey();
        // A Token-2022 account carries the extensions its mint requires (a transfer fee's withheld amount, say).
        let len = if a.program == spl_token_2022::ID {
            let data = self.svm.get_account(&a.mint).unwrap().data;
            let kinds = StateWithExtensions::<spl_token_2022::state::Mint>::unpack(&data).unwrap().get_extension_types().unwrap();
            ExtensionType::try_calculate_account_len::<spl_token_2022::state::Account>(&ExtensionType::get_required_init_account_extensions(&kinds)).unwrap()
        } else { 165 };
        let mut ixs = vec![system_instruction::create_account(&payer, &account.pubkey(), rent.minimum_balance(len), len as u64, &a.program),
            spl_token_2022::instruction::initialize_account3(&a.program, &account.pubkey(), &a.mint, &owner).unwrap()];
        if amount > 0 { ixs.push(spl_token_2022::instruction::mint_to(&a.program, &a.mint, &account.pubkey(), &payer, &[], amount).unwrap()); }
        self.send(ixs, &[&account]).unwrap();
        account.pubkey()
    }
    fn create_ix(&self, a: Asset, creator: Pubkey, token: Pubkey, sol: u64, tokens: u64, revision: u64) -> Instruction {
        ix(instruction::CreatePool { sol, tokens, expected_config_revision: revision }, accounts::CreatePool {
            creator, config: self.config, mint: a.mint, pool: a.pool(), vault: a.vault(), creator_token: token, position: a.position(&creator),
            launchpad_market: a.launchpad_market(), token_program: a.program, system_program: solana_sdk::system_program::ID, event_authority: event_authority(), program: ID })
    }
    fn swap_ix(&self, a: Asset, mode: &str, amount: u64, bound: u64, trader: Pubkey, token: Pubkey) -> Instruction {
        let accs = accounts::Swap { trader, pool: a.pool(), mint: a.mint, vault: a.vault(), trader_token: token, protocol: a.protocol(), token_program: a.program, system_program: solana_sdk::system_program::ID, event_authority: event_authority(), program: ID };
        match mode {
            "buyExactIn" => ix(instruction::BuyExactIn { gross: amount, min_tokens: bound }, accs),
            "buyExactOut" => ix(instruction::BuyExactOut { tokens: amount, max_gross: bound }, accs),
            "sellExactIn" => ix(instruction::SellExactIn { tokens: amount, min_sol: bound }, accs),
            "sellExactOut" => ix(instruction::SellExactOut { sol: amount, max_tokens: bound }, accs),
            _ => panic!("unknown mode"),
        }
    }
    fn add_ix(&self, a: Asset, owner: Pubkey, token: Pubkey, shares: u64, max_sol: u64, max_tokens: u64) -> Instruction {
        ix(instruction::AddLiquidity { shares, max_sol, max_tokens }, accounts::AddLiquidity { owner, pool: a.pool(), mint: a.mint, vault: a.vault(), owner_token: token, position: a.position(&owner), token_program: a.program, system_program: solana_sdk::system_program::ID, event_authority: event_authority(), program: ID })
    }
    fn remove_ix(&self, a: Asset, owner: Pubkey, token: Pubkey, shares: u64, min_sol: u64, min_tokens: u64) -> Instruction {
        ix(instruction::RemoveLiquidity { shares, min_sol, min_tokens }, accounts::RemoveLiquidity { owner, pool: a.pool(), mint: a.mint, vault: a.vault(), owner_token: token, position: a.position(&owner), token_program: a.program, event_authority: event_authority(), program: ID })
    }
    /// SOL reserves are the pool's lamports above rent, token reserves its vault, shares the positions' sum.
    fn assert_books(&self, a: Asset, owners: &[Pubkey]) {
        let pool = self.pool(a);
        assert_eq!(self.balance(a.pool()), self.rent(a.pool()) + pool.sol_reserve, "SOL reserve");
        assert_eq!(self.tokens(a.vault()), pool.token_reserve, "token reserve");
        let held: u64 = owners.iter().filter_map(|o| self.svm.get_account(&a.position(o)).map(|_| self.decode::<Position>(a.position(o)).shares)).sum();
        assert_eq!(held, pool.shares, "shares");
    }
    /// A pool opened by a fresh creator; returns the creator and their token account.
    fn open(&mut self, a: Asset, sol: u64, tokens: u64) -> (Keypair, Pubkey) {
        let creator = self.wallet(sol + 10 * SOL); let token = self.holder(a, creator.pubkey(), tokens);
        let create = self.create_ix(a, creator.pubkey(), token, sol, tokens, 0); self.send(vec![create], &[&creator]).unwrap();
        (creator, token)
    }
}

/// The swap model, written apart from the program's math module.
struct Model { gross: u64, net: u64, tokens: u64, protocol: u64, lp: u64, sol_after: u64, token_after: u64 }
fn fee(gross: u64) -> (u64, u64, u64) {
    let total = gross as u128 * TERMS.fee_bps as u128 / 10_000; let protocol = total * TERMS.protocol_share_bps as u128 / 10_000;
    (total as u64, protocol as u64, (total - protocol) as u64)
}
fn gross_for(net: u64) -> u64 {
    let ok = |g: u64| g - fee(g).0 >= net;
    let mut g = ((net as u128 * 10_000).div_ceil(10_000 - TERMS.fee_bps as u128)) as u64;
    while g > net && ok(g - 1) { g -= 1; }
    assert!(ok(g)); g
}
fn model(mode: &str, amount: u64, x: u64, y: u64) -> Model {
    let (x1, y1) = (x as u128, y as u128);
    match mode {
        "buyExactIn" => { let (t, p, l) = fee(amount); let out = (y1 * (amount - t) as u128 / (x1 + (amount - t) as u128)) as u64; Model { gross: amount, net: amount - t, tokens: out, protocol: p, lp: l, sol_after: x + amount - p, token_after: y - out } }
        "buyExactOut" => { let net = (x1 * amount as u128).div_ceil(y1 - amount as u128) as u64; let g = gross_for(net); let (t, p, l) = fee(g); Model { gross: g, net: g - t, tokens: amount, protocol: p, lp: l, sol_after: x + g - p, token_after: y - amount } }
        "sellExactIn" => { let g = (x1 * amount as u128 / (y1 + amount as u128)) as u64; let (t, p, l) = fee(g); Model { gross: g, net: g - t, tokens: amount, protocol: p, lp: l, sol_after: x - (g - t) - p, token_after: y + amount } }
        "sellExactOut" => { let g = gross_for(amount); let input = (y1 * g as u128).div_ceil(x1 - g as u128) as u64; let (_, p, l) = fee(g); Model { gross: g, net: amount, tokens: input, protocol: p, lp: l, sol_after: x - amount - p, token_after: y + input } }
        _ => panic!(),
    }
}
fn isqrt(n: u128) -> u64 { let mut r = (n as f64).sqrt() as u128; while r * r > n { r -= 1; } while (r + 1) * (r + 1) <= n { r += 1; } r as u64 }

#[test]
fn error_codes_are_stable() {
    assert_eq!(u32::from(fix_amm::AmmError::InvalidTerms), 6000);
    assert_eq!(u32::from(fix_amm::AmmError::SwapNotOpen), 6008);
    assert_eq!(u32::from(fix_amm::AmmError::InsufficientShares), 6012);
    assert_eq!(u32::from(fix_amm::AmmError::PairOrder), 6013);
}

#[test]
fn creation_seeds_the_pool_and_its_slot_takes_no_swap_from_anyone() {
    for kind in [Kind::Spl, Kind::Token2022Metadata] {
        let mut h = Harness::new(); let a = h.mint(kind);
        let creator = h.wallet(100 * SOL); let token = h.holder(a, creator.pubkey(), 2_000_000 * TOKEN);
        let stranger = h.wallet(100 * SOL); let stranger_token = h.holder(a, stranger.pubkey(), 50_000 * TOKEN);
        let (sol, tokens) = (10 * SOL, 1_000_000 * TOKEN);
        // A swap in the creation transaction fails the whole transaction: nothing is created.
        for (mode, amount, bound) in [("buyExactIn", SOL, 1), ("buyExactOut", 1_000 * TOKEN, u64::MAX), ("sellExactIn", 1_000 * TOKEN, 0), ("sellExactOut", SOL / 10, u64::MAX)] {
            let create = h.create_ix(a, creator.pubkey(), token, sol, tokens, 0); let swap = h.swap_ix(a, mode, amount, bound, creator.pubkey(), token);
            let refused = h.send(vec![create, swap], &[&creator]).unwrap_err();
            assert!(refused.contains("SwapNotOpen"), "{mode}: {refused}");
            assert!(h.svm.get_account(&a.pool()).is_none() && h.svm.get_account(&a.vault()).is_none());
        }
        let before = h.balance(creator.pubkey());
        let create = h.create_ix(a, creator.pubkey(), token, sol, tokens, 0); let created = h.send(vec![create], &[&creator]).unwrap();
        let pool = h.pool(a); let shares = isqrt(sol as u128 * tokens as u128);
        assert_eq!((pool.mint, pool.token_program, pool.creator, pool.terms), (a.mint, a.program, creator.pubkey(), TERMS));
        assert_eq!((pool.sol_reserve, pool.token_reserve, pool.shares, pool.created_slot, pool.seeded_slot), (sol, tokens, shares, h.slot(), h.slot()));
        let r = receipt(&created);
        assert_eq!((r.action, r.gross, r.net, r.tokens, r.shares, r.total_shares, r.protocol_fee, r.lp_fee, r.price_before), (0, sol, sol, tokens, shares, shares, 0, 0, 0));
        assert_eq!(r.price_after, ((sol as u128) << 64) / tokens as u128);
        let created_event = events(&created).into_iter().find(|b| b.starts_with(PoolCreated::DISCRIMINATOR)).unwrap();
        let ev = PoolCreated::deserialize(&mut &created_event[8..]).unwrap();
        assert_eq!((ev.mint, ev.token_program, ev.creator, ev.decimals, ev.created_slot), (a.mint, a.program, creator.pubkey(), 6, h.slot()));
        // The creator paid the deposit and the rent of the pool, its vault and their position; no fee.
        let rents = h.rent(a.pool()) + h.rent(a.vault()) + h.rent(a.position(&creator.pubkey()));
        assert_eq!(before - h.balance(creator.pubkey()), sol + rents);
        h.assert_books(a, &[creator.pubkey()]);
        // Every swap in the creation slot is refused, the creator's and a stranger's.
        for (who, acct) in [(&creator, token), (&stranger, stranger_token)] {
            for (mode, amount, bound) in [("buyExactIn", SOL, 1), ("buyExactOut", 1_000 * TOKEN, u64::MAX), ("sellExactIn", 1_000 * TOKEN, 0), ("sellExactOut", SOL / 10, u64::MAX)] {
                let swap = h.swap_ix(a, mode, amount, bound, who.pubkey(), acct);
                let refused = h.send_same_slot(vec![swap], &[who]).unwrap_err();
                assert!(refused.contains("SwapNotOpen"), "{mode}: {refused}");
            }
        }
        // From the next slot every mode trades.
        for (mode, amount, bound) in [("buyExactIn", SOL, 1), ("buyExactOut", 1_000 * TOKEN, u64::MAX), ("sellExactIn", 1_000 * TOKEN, 0), ("sellExactOut", SOL / 10, u64::MAX)] {
            let swap = h.swap_ix(a, mode, amount, bound, stranger.pubkey(), stranger_token); h.send(vec![swap], &[&stranger]).unwrap();
        }
        h.assert_books(a, &[creator.pubkey()]);
    }
}

#[test]
fn swaps_in_four_modes_match_the_model_and_book_every_lamport() {
    let mut vectors = vec![];
    for kind in [Kind::Spl, Kind::Token2022Metadata] {
        let mut h = Harness::new(); let a = h.mint(kind);
        let (creator, _) = h.open(a, 25 * SOL, 3_000_000 * TOKEN);
        let trader = h.wallet(500 * SOL); let token = h.holder(a, trader.pubkey(), 900_000 * TOKEN);
        let mut accrued = 0u64;
        let plan: [(&str, u64); 12] = [("buyExactIn", 3 * SOL + 7), ("sellExactIn", 41_234 * TOKEN + 9), ("buyExactOut", 250_001 * TOKEN), ("sellExactOut", 2 * SOL + 3),
            ("buyExactIn", 1), ("buyExactIn", 101), ("sellExactIn", 7), ("sellExactOut", 99), ("buyExactOut", 1), ("buyExactIn", 120 * SOL), ("sellExactIn", 700_000 * TOKEN), ("sellExactOut", 9 * SOL)];
        for (mode, amount) in plan {
            let before = h.pool(a); let m = model(mode, amount, before.sol_reserve, before.token_reserve);
            if m.tokens == 0 || m.net == 0 {
                let swap = h.swap_ix(a, mode, amount, if mode.ends_with("In") { 0 } else { u64::MAX }, trader.pubkey(), token);
                assert!(h.send(vec![swap], &[&trader]).unwrap_err().contains("SlippageExceeded"), "{mode} {amount}"); continue;
            }
            // The bound the trade must meet: exactly the model's figure passes, one atom tighter fails.
            let bound = match mode { "buyExactIn" => m.tokens, "buyExactOut" => m.gross, "sellExactIn" => m.net, _ => m.tokens };
            let tighter = match mode { "buyExactIn" | "sellExactIn" => bound + 1, _ => bound - 1 };
            let refused = h.swap_ix(a, mode, amount, tighter, trader.pubkey(), token);
            // A one-lamport exact-out leaves a zero maximum, which is refused before any slippage check.
            let expected = if tighter == 0 && mode.ends_with("Out") { "ZeroAmount" } else { "SlippageExceeded" };
            assert!(h.send(vec![refused], &[&trader]).unwrap_err().contains(expected), "{mode}");
            let (sol0, tok0, shard0) = (h.balance(trader.pubkey()), h.tokens(token), h.balance(a.protocol()));
            let swap = h.swap_ix(a, mode, amount, bound, trader.pubkey(), token); let meta = h.send(vec![swap], &[&trader]).unwrap(); let r = receipt(&meta);
            let after = h.pool(a);
            assert_eq!((r.gross, r.net, r.tokens, r.protocol_fee, r.lp_fee), (m.gross, m.net, m.tokens, m.protocol, m.lp), "{mode} {amount}");
            assert_eq!((after.sol_reserve, after.token_reserve), (m.sol_after, m.token_after), "{mode} {amount}");
            assert_eq!((r.sol_reserve, r.token_reserve, r.total_shares, r.shares), (after.sol_reserve, after.token_reserve, after.shares, 0));
            assert_eq!(r.protocol_fee + r.lp_fee, fee(r.gross).0);
            // What the trader paid and got is the receipt, to the lamport and the atom.
            if mode.starts_with("buy") {
                assert_eq!(r.action, 1); assert_eq!(sol0 - h.balance(trader.pubkey()), r.gross); assert_eq!(h.tokens(token) - tok0, r.tokens);
            } else {
                assert_eq!(r.action, 2); assert_eq!(h.balance(trader.pubkey()) - sol0, r.net); assert_eq!(tok0 - h.tokens(token), r.tokens);
            }
            assert_eq!(h.balance(a.protocol()) - shard0, r.protocol_fee);
            accrued += r.protocol_fee;
            // The constant product never falls: the liquidity providers' part and every rounding stay in it.
            assert!(after.sol_reserve as u128 * after.token_reserve as u128 >= before.sol_reserve as u128 * before.token_reserve as u128, "{mode}");
            assert_eq!(r.price_before, ((before.sol_reserve as u128) << 64) / before.token_reserve as u128);
            assert_eq!(r.price_after, ((after.sol_reserve as u128) << 64) / after.token_reserve as u128);
            h.assert_books(a, &[creator.pubkey()]);
            vectors.push(json!({"token":format!("{kind:?}"),"mode":mode,"amount":amount.to_string(),"bound":bound.to_string(),
                "before":{"solReserve":before.sol_reserve.to_string(),"tokenReserve":before.token_reserve.to_string(),"shares":before.shares.to_string()},
                "after":{"solReserve":after.sol_reserve.to_string(),"tokenReserve":after.token_reserve.to_string(),"shares":after.shares.to_string()},"receipt":receipt_json(&r)}));
        }
        let protocol: Protocol = h.decode(a.protocol());
        assert_eq!(protocol.accrued, accrued); assert_eq!(h.balance(a.protocol()), h.rent(a.protocol()) + accrued);
        // Selling more SOL than the pool holds is refused by name.
        let swap = h.swap_ix(a, "sellExactOut", h.pool(a).sol_reserve, u64::MAX, trader.pubkey(), token);
        assert!(h.send(vec![swap], &[&trader]).unwrap_err().contains("InsufficientLiquidity"));
        let swap = h.swap_ix(a, "buyExactOut", h.pool(a).token_reserve, u64::MAX, trader.pubkey(), token);
        assert!(h.send(vec![swap], &[&trader]).unwrap_err().contains("InsufficientLiquidity"));
    }
    let file = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/amm-chain-vectors.json");
    std::fs::write(file, serde_json::to_string_pretty(&json!({"source":"LiteSVM executed fix_amm SBF receipts","terms":{"feeBps":TERMS.fee_bps,"protocolShareBps":TERMS.protocol_share_bps},"vectors":vectors})).unwrap()).unwrap();
}

#[test]
fn liquidity_moves_at_the_pool_ratio_and_every_rounding_stays_in_the_pool() {
    let mut h = Harness::new(); let a = h.mint(Kind::Token2022Metadata);
    let (creator, creator_token) = h.open(a, 40 * SOL, 8_000_000 * TOKEN);
    let lp = h.wallet(200 * SOL); let lp_token = h.holder(a, lp.pubkey(), 9_000_000 * TOKEN);
    let pool = h.pool(a);
    // Exactly `shares`, each reserve's part rounded up; what the budget doesn't need stays in the wallet.
    let shares = pool.shares / 3 + 7;
    let sol = (shares as u128 * pool.sol_reserve as u128).div_ceil(pool.shares as u128) as u64;
    let tokens = (shares as u128 * pool.token_reserve as u128).div_ceil(pool.shares as u128) as u64;
    let short = h.add_ix(a, lp.pubkey(), lp_token, shares, sol - 1, u64::MAX);
    assert!(h.send(vec![short], &[&lp]).unwrap_err().contains("SlippageExceeded"));
    let short = h.add_ix(a, lp.pubkey(), lp_token, shares, u64::MAX, tokens - 1);
    assert!(h.send(vec![short], &[&lp]).unwrap_err().contains("SlippageExceeded"));
    let (sol0, tok0) = (h.balance(lp.pubkey()), h.tokens(lp_token));
    let added = h.add_ix(a, lp.pubkey(), lp_token, shares, sol + 5 * SOL, tokens + 1_000 * TOKEN);
    let r = receipt(&h.send(vec![added], &[&lp]).unwrap());
    assert_eq!((r.action, r.gross, r.net, r.tokens, r.shares, r.total_shares), (3, sol, sol, tokens, shares, pool.shares + shares));
    let position_rent = h.rent(a.position(&lp.pubkey()));
    assert_eq!(sol0 - h.balance(lp.pubkey()), sol + position_rent); assert_eq!(tok0 - h.tokens(lp_token), tokens);
    let position: Position = h.decode(a.position(&lp.pubkey()));
    assert_eq!((position.pool, position.owner, position.shares), (a.pool(), lp.pubkey(), shares));
    // A second add into the same position pays no rent again.
    let sol0 = h.balance(lp.pubkey()); let more = h.add_ix(a, lp.pubkey(), lp_token, 1_000, u64::MAX, u64::MAX);
    let r = receipt(&h.send(vec![more], &[&lp]).unwrap()); assert_eq!(sol0 - h.balance(lp.pubkey()), r.gross);
    h.assert_books(a, &[creator.pubkey(), lp.pubkey()]);
    // Swaps leave their liquidity providers' part in the pool: each share is worth more of both reserves.
    let trader = h.wallet(1_000 * SOL); let trader_token = h.holder(a, trader.pubkey(), 0);
    let per_share = |p: &Pool| (p.sol_reserve as f64 * p.token_reserve as f64).sqrt() / p.shares as f64;
    let start = per_share(&h.pool(a));
    for _ in 0..6 {
        let buy = h.swap_ix(a, "buyExactIn", 30 * SOL, 1, trader.pubkey(), trader_token); h.send(vec![buy], &[&trader]).unwrap();
        let held = h.tokens(trader_token); let sell = h.swap_ix(a, "sellExactIn", held, 0, trader.pubkey(), trader_token); h.send(vec![sell], &[&trader]).unwrap();
    }
    assert!(per_share(&h.pool(a)) > start * 1.005, "LP fees accrue to shares");
    // Removing rounds each reserve's part down; too high a minimum or too many shares is refused by name.
    let position: Position = h.decode(a.position(&lp.pubkey())); let pool = h.pool(a);
    let out_sol = (position.shares as u128 * pool.sol_reserve as u128 / pool.shares as u128) as u64;
    let out_tokens = (position.shares as u128 * pool.token_reserve as u128 / pool.shares as u128) as u64;
    let greedy = h.remove_ix(a, lp.pubkey(), lp_token, position.shares, out_sol + 1, 0);
    assert!(h.send(vec![greedy], &[&lp]).unwrap_err().contains("SlippageExceeded"));
    let too_many = h.remove_ix(a, lp.pubkey(), lp_token, position.shares + 1, 0, 0);
    assert!(h.send(vec![too_many], &[&lp]).unwrap_err().contains("InsufficientShares"));
    let (sol0, tok0) = (h.balance(lp.pubkey()), h.tokens(lp_token));
    let removed = h.remove_ix(a, lp.pubkey(), lp_token, position.shares, out_sol, out_tokens);
    let r = receipt(&h.send(vec![removed], &[&lp]).unwrap());
    assert_eq!((r.action, r.gross, r.tokens, r.shares), (4, out_sol, out_tokens, position.shares));
    assert_eq!(h.balance(lp.pubkey()) - sol0, out_sol); assert_eq!(h.tokens(lp_token) - tok0, out_tokens);
    h.assert_books(a, &[creator.pubkey(), lp.pubkey()]);
    // The last shares take everything: nothing stays behind in an emptied pool.
    let creator_shares = h.decode::<Position>(a.position(&creator.pubkey())).shares; let pool = h.pool(a);
    let (sol0, tok0) = (h.balance(creator.pubkey()), h.tokens(creator_token));
    let all = h.remove_ix(a, creator.pubkey(), creator_token, creator_shares, pool.sol_reserve, pool.token_reserve);
    let r = receipt(&h.send(vec![all], &[&creator]).unwrap());
    assert_eq!((r.gross, r.tokens, r.total_shares, r.sol_reserve, r.token_reserve, r.price_after), (pool.sol_reserve, pool.token_reserve, 0, 0, 0, 0));
    assert_eq!(h.balance(creator.pubkey()) - sol0, pool.sol_reserve); assert_eq!(h.tokens(creator_token) - tok0, pool.token_reserve);
    assert_eq!(h.balance(a.pool()), h.rent(a.pool())); assert_eq!(h.tokens(a.vault()), 0);
    let swap = h.swap_ix(a, "buyExactIn", SOL, 1, trader.pubkey(), trader_token);
    assert!(h.send(vec![swap], &[&trader]).unwrap_err().contains("InsufficientLiquidity"));
    // Anyone can seed the empty pool again; the amounts set the price and swaps wait for the next slot.
    let seeder = h.wallet(50 * SOL); let seeder_token = h.holder(a, seeder.pubkey(), 500_000 * TOKEN);
    let want = isqrt(3 * SOL as u128 * 400_000 * TOKEN as u128);
    let greedy = h.add_ix(a, seeder.pubkey(), seeder_token, want + 1, 3 * SOL, 400_000 * TOKEN);
    assert!(h.send(vec![greedy], &[&seeder]).unwrap_err().contains("SlippageExceeded"));
    let seed = h.add_ix(a, seeder.pubkey(), seeder_token, want, 3 * SOL, 400_000 * TOKEN);
    let r = receipt(&h.send(vec![seed], &[&seeder]).unwrap());
    assert_eq!((r.gross, r.tokens, r.shares, r.total_shares, r.price_before), (3 * SOL, 400_000 * TOKEN, want, want, 0));
    assert_eq!(h.pool(a).seeded_slot, h.slot());
    let swap = h.swap_ix(a, "buyExactIn", SOL, 1, trader.pubkey(), trader_token);
    assert!(h.send_same_slot(vec![swap.clone()], &[&trader]).unwrap_err().contains("SwapNotOpen"));
    h.send(vec![swap], &[&trader]).unwrap();
    h.assert_books(a, &[creator.pubkey(), lp.pubkey(), seeder.pubkey()]);
}

#[test]
fn coarse_shares_cost_a_depositor_nothing_beyond_one_atom() {
    // A pool opened with 4 shares and then made rich: each share is worth a great deal.
    let mut h = Harness::new(); let a = h.mint(Kind::Spl);
    let (creator, _) = h.open(a, 7, 3);
    let trader = h.wallet(10 * SOL); let token = h.holder(a, trader.pubkey(), 0);
    for _ in 0..4 { let buy = h.swap_ix(a, "buyExactIn", 1_000, 1, trader.pubkey(), token); let _ = h.send(vec![buy], &[&trader]); }
    let pool = h.pool(a); assert_eq!(pool.shares, 4);
    let depositor = h.wallet(10 * SOL); let dep_token = h.holder(a, depositor.pubkey(), 1_000);
    let (sol0, tok0) = (h.balance(depositor.pubkey()), h.tokens(dep_token));
    let add = h.add_ix(a, depositor.pubkey(), dep_token, 3, SOL, 1_000); let r = receipt(&h.send(vec![add], &[&depositor]).unwrap());
    let rent = h.rent(a.position(&depositor.pubkey()));
    assert_eq!(sol0 - h.balance(depositor.pubkey()) - rent, r.gross); assert_eq!(tok0 - h.tokens(dep_token), r.tokens);
    // Taking the same shares straight back returns what was paid, less under one atom of each.
    let remove = h.remove_ix(a, depositor.pubkey(), dep_token, 3, 0, 0); let back = receipt(&h.send(vec![remove], &[&depositor]).unwrap());
    assert!(r.gross - back.gross <= 1 && r.tokens - back.tokens <= 1, "paid {} {} got {} {}", r.gross, r.tokens, back.gross, back.tokens);
    h.assert_books(a, &[creator.pubkey(), depositor.pubkey()]);
}

#[test]
fn mints_and_admin_actions_are_refused_by_name() {
    let mut h = Harness::new();
    // Transfer fees, permanent delegates and close authorities would let a transfer or the vault change outside the pool.
    for extension in [ExtensionType::TransferFeeConfig, ExtensionType::PermanentDelegate, ExtensionType::MintCloseAuthority] {
        let a = h.mint(Kind::Token2022(extension)); let creator = h.wallet(20 * SOL);
        let token = h.holder(a, creator.pubkey(), 0);
        let create = h.create_ix(a, creator.pubkey(), token, SOL, 1, 0);
        assert!(h.send(vec![create], &[&creator]).unwrap_err().contains("UnsupportedMint"), "{extension:?}");
    }
    // A display-only extension is accepted.
    let interest = h.mint(Kind::Token2022(ExtensionType::InterestBearingConfig)); h.open(interest, SOL, 1_000 * TOKEN);
    // Wrapped SOL against SOL is refused.
    let native = Asset { mint: spl_token::native_mint::ID, program: spl_token::ID };
    let mut data = vec![0u8; 82];
    spl_token::state::Mint { mint_authority: None.into(), supply: 0, decimals: 9, is_initialized: true, freeze_authority: None.into() }.pack_into_slice(&mut data);
    h.svm.set_account(native.mint, Account { lamports: 1_000_000_000, data, owner: spl_token::ID, executable: false, rent_epoch: 0 }).unwrap();
    let creator = h.wallet(20 * SOL); let token = h.holder(native, creator.pubkey(), 0);
    let create = h.create_ix(native, creator.pubkey(), token, SOL, 1, 0);
    assert!(h.send(vec![create], &[&creator]).unwrap_err().contains("UnsupportedMint"));
    // A coin the launchpad created has a market there: it trades only in that market.
    let coin = h.mint(Kind::Token2022Metadata);
    h.svm.set_account(coin.launchpad_market(), Account { lamports: 2_000_000, data: vec![1; 222], owner: LAUNCHPAD_ID, executable: false, rent_epoch: 0 }).unwrap();
    let creator = h.wallet(20 * SOL); let token = h.holder(coin, creator.pubkey(), 1_000 * TOKEN);
    let create = h.create_ix(coin, creator.pubkey(), token, SOL, 1_000 * TOKEN, 0);
    assert!(h.send(vec![create], &[&creator]).unwrap_err().contains("LaunchpadCoin"));
    // SOL sent to that address beforehand changes nothing: only an account the launchpad owns counts.
    let outside = h.mint(Kind::Spl);
    h.svm.set_account(outside.launchpad_market(), Account { lamports: 2_000_000, data: vec![], owner: solana_sdk::system_program::ID, executable: false, rent_epoch: 0 }).unwrap();
    let (_creator, _) = h.open(outside, SOL, 1_000 * TOKEN);
    // One pool per mint.
    let again = h.wallet(20 * SOL); let token = h.holder(outside, again.pubkey(), 1_000 * TOKEN);
    let create = h.create_ix(outside, again.pubkey(), token, SOL, 1_000 * TOKEN, 0);
    assert!(h.send(vec![create], &[&again]).is_err());
    // Config: only the admin changes it, a change reaches only later pools, and a pause stops only creation.
    let stranger = h.wallet(SOL);
    let update = |admin: Pubkey, terms: Terms, paused: bool| ix(instruction::UpdateConfig { terms, paused }, accounts::AdminConfig { admin, config: pda(&[b"config"]) });
    assert!(h.send(vec![update(stranger.pubkey(), TERMS, true)], &[&stranger]).unwrap_err().contains("Unauthorized"));
    let admin = h.admin.insecure_clone();
    assert!(h.send(vec![update(admin.pubkey(), Terms { fee_bps: 10_000, protocol_share_bps: 0 }, false)], &[&admin]).unwrap_err().contains("InvalidTerms"));
    let newer = Terms { fee_bps: 30, protocol_share_bps: 2_000 };
    h.send(vec![update(admin.pubkey(), newer, true)], &[&admin]).unwrap();
    let config: Config = h.decode(h.config); assert_eq!((config.revision, config.paused, config.terms), (1, true, newer));
    assert_eq!(h.pool(outside).terms, TERMS, "a live pool keeps the terms it was created with");
    let later = h.mint(Kind::Spl); let creator = h.wallet(20 * SOL); let token = h.holder(later, creator.pubkey(), 1_000 * TOKEN);
    assert!(h.send(vec![h.create_ix(later, creator.pubkey(), token, SOL, 1_000 * TOKEN, 1)], &[&creator]).unwrap_err().contains("CreationPaused"));
    let trader = h.wallet(10 * SOL); let trader_token = h.holder(outside, trader.pubkey(), 0);
    h.send(vec![h.swap_ix(outside, "buyExactIn", SOL / 10, 1, trader.pubkey(), trader_token)], &[&trader]).unwrap();
    h.send(vec![update(admin.pubkey(), newer, false)], &[&admin]).unwrap();
    assert!(h.send(vec![h.create_ix(later, creator.pubkey(), token, SOL, 1_000 * TOKEN, 1)], &[&creator]).unwrap_err().contains("ConfigChanged"));
    h.send(vec![h.create_ix(later, creator.pubkey(), token, SOL, 1_000 * TOKEN, 2)], &[&creator]).unwrap();
    assert_eq!(h.pool(later).terms, newer);
    // Admin hand-over takes the new admin's signature.
    let next = h.wallet(SOL);
    h.send(vec![ix(instruction::ProposeAdmin { new_admin: next.pubkey() }, accounts::AdminConfig { admin: admin.pubkey(), config: h.config })], &[&admin]).unwrap();
    assert!(h.send(vec![ix(instruction::AcceptAdmin {}, accounts::AcceptAdmin { pending_admin: stranger.pubkey(), config: h.config })], &[&stranger]).unwrap_err().contains("Unauthorized"));
    h.send(vec![ix(instruction::AcceptAdmin {}, accounts::AcceptAdmin { pending_admin: next.pubkey(), config: h.config })], &[&next]).unwrap();
    assert_eq!(h.decode::<Config>(h.config).admin, next.pubkey());
    // Shards stop at 63.
    assert!(h.send(vec![h.shard_ix(PROTOCOL_SHARDS)], &[]).unwrap_err().contains("InvalidProtocolShard"));
    // Only the upgrade authority initializes the config.
    let mut fresh = Harness::uninitialized(); let intruder = fresh.wallet(SOL);
    assert!(fresh.send(vec![fresh.init_ix(intruder.pubkey())], &[&intruder]).unwrap_err().contains("Unauthorized"));
}

#[test]
fn compute_units_per_action_are_measured() {
    let mut h = Harness::new();
    let cu = |m: &TransactionMetadata| m.compute_units_consumed;
    let spl = h.mint(Kind::Spl); let t22 = h.mint(Kind::Token2022Metadata);
    let mut report = json!({"source":"LiteSVM 0.6.1, compiled fix_amm SBF, launch terms; each figure includes the 1.4M SetComputeUnitLimit instruction"});
    for (name, a) in [("spl", spl), ("token2022", t22)] {
        let creator = h.wallet(100 * SOL); let token = h.holder(a, creator.pubkey(), 5_000_000 * TOKEN);
        let create = h.create_ix(a, creator.pubkey(), token, 30 * SOL, 3_000_000 * TOKEN, 0); let created = cu(&h.send(vec![create], &[&creator]).unwrap());
        let trader = h.wallet(100 * SOL); let tt = h.holder(a, trader.pubkey(), 500_000 * TOKEN);
        let mut buy = 0; let mut sell = 0;
        for (mode, amount, bound) in [("buyExactIn", SOL, 1), ("buyExactOut", 10_000 * TOKEN, u64::MAX), ("sellExactIn", 10_000 * TOKEN, 1), ("sellExactOut", SOL / 2, u64::MAX)] {
            let units = cu(&h.send(vec![h.swap_ix(a, mode, amount, bound, trader.pubkey(), tt)], &[&trader]).unwrap());
            if mode.starts_with("buy") { buy = buy.max(units) } else { sell = sell.max(units) }
        }
        let add_new = cu(&h.send(vec![h.add_ix(a, trader.pubkey(), tt, 1_000_000, u64::MAX, u64::MAX)], &[&trader]).unwrap());
        let add_more = cu(&h.send(vec![h.add_ix(a, trader.pubkey(), tt, 1_000_000, u64::MAX, u64::MAX)], &[&trader]).unwrap());
        let remove = cu(&h.send(vec![h.remove_ix(a, trader.pubkey(), tt, 1_500_000, 0, 0)], &[&trader]).unwrap());
        report[name] = json!({"createPool":created,"buy":buy,"sell":sell,"addLiquidityNewPosition":add_new,"addLiquidity":add_more,"removeLiquidity":remove});
    }
    std::fs::write(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/amm-compute-units.json"), serde_json::to_string_pretty(&report).unwrap()).unwrap();
    println!("Compute units: {report}");
    // sdk/src/amm/compute.ts carries these ceilings; a measured action above one fails here first.
    for name in ["spl", "token2022"] {
        for (field, ceiling) in [("createPool", 90_000u64), ("buy", 60_000), ("sell", 60_000), ("addLiquidityNewPosition", 60_000), ("addLiquidity", 50_000), ("removeLiquidity", 50_000)] {
            assert!(report[name][field].as_u64().unwrap() <= ceiling, "{name}.{field} exceeds {ceiling}: {report}");
        }
    }
}

#[test]
fn transactions_are_exported_for_sdk_and_indexer_regressions() {
    let mut h = Harness::new(); let a = h.mint(Kind::Token2022Metadata);
    let creator = h.wallet(100 * SOL); let creator_token = h.holder(a, creator.pubkey(), 3_000_000 * TOKEN);
    let mut selected = vec![];
    let create = h.create_ix(a, creator.pubkey(), creator_token, 12 * SOL, 2_000_000 * TOKEN, 0); h.send(vec![create], &[&creator]).unwrap(); selected.push((h.recorded.len() - 1, "create_pool"));
    let trader = h.wallet(100 * SOL); let token = h.holder(a, trader.pubkey(), 0);
    for (mode, amount, bound) in [("buyExactIn", 2 * SOL, 1), ("buyExactOut", 5_000 * TOKEN, u64::MAX), ("sellExactIn", 4_000 * TOKEN, 1), ("sellExactOut", SOL / 4, u64::MAX)] {
        let swap = h.swap_ix(a, mode, amount, bound, trader.pubkey(), token); h.send(vec![swap], &[&trader]).unwrap(); selected.push((h.recorded.len() - 1, mode));
    }
    let lp = h.wallet(100 * SOL); let lp_token = h.holder(a, lp.pubkey(), 1_000_000 * TOKEN);
    let add = h.add_ix(a, lp.pubkey(), lp_token, 50_000_000, u64::MAX, u64::MAX); h.send(vec![add], &[&lp]).unwrap(); selected.push((h.recorded.len() - 1, "add_liquidity"));
    let remove = h.remove_ix(a, lp.pubkey(), lp_token, 20_000_000, 0, 0); h.send(vec![remove], &[&lp]).unwrap(); selected.push((h.recorded.len() - 1, "remove_liquidity"));
    // Two swaps in one transaction keep both receipts.
    let one = h.swap_ix(a, "buyExactIn", SOL, 1, trader.pubkey(), token); let two = h.swap_ix(a, "buyExactIn", SOL, 1, trader.pubkey(), token);
    let both = h.send(vec![one, two], &[&trader]).unwrap(); assert_eq!(receipts(&both).len(), 2); selected.push((h.recorded.len() - 1, "two_buys"));
    let transactions: Vec<Value> = selected.into_iter().map(|(index, name)| {
        let mut item = h.recorded[index].clone(); item["name"] = json!(name);
        assert!(!item["logs"].as_array().unwrap().iter().any(|l| l.as_str().unwrap().starts_with("Program data: ")), "events never use the log budget");
        let mut names = vec![]; let mut list = vec![];
        for group in item["innerInstructions"].as_array().unwrap() { for entry in group["instructions"].as_array().unwrap() {
            let data = STANDARD.decode(entry["dataBase64"].as_str().unwrap()).unwrap();
            if let Some(bytes) = data.strip_prefix(anchor_lang::event::EVENT_IX_TAG_LE) {
                if bytes.starts_with(PoolCreated::DISCRIMINATOR) { names.push("PoolCreated"); }
                else if bytes.starts_with(Receipt::DISCRIMINATOR) { names.push("Receipt"); list.push(receipt_json(&Receipt::deserialize(&mut &bytes[8..]).unwrap())); }
                else { panic!("unexpected event"); }
            }
        } }
        item["expectedEvents"] = json!(names); item["expectedReceipts"] = json!(list); item
    }).collect();
    let keys = [h.config, a.pool(), a.vault(), a.mint, a.protocol(), a.position(&creator.pubkey()), a.position(&lp.pubkey())];
    let accounts: Vec<Value> = keys.iter().filter_map(|k| h.svm.get_account(k).map(|acc| json!({"address":k.to_string(),"owner":acc.owner.to_string(),"lamports":acc.lamports.to_string(),"dataBase64":STANDARD.encode(&acc.data)}))).collect();
    let file = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/amm-event-log-vectors.json");
    std::fs::write(file, serde_json::to_string_pretty(&json!({"format":"fix-amm-litesvm-v1","programId":ID.to_string(),"mint":a.mint.to_string(),"tokenProgram":a.program.to_string(),
        "pool":a.pool().to_string(),"creator":creator.pubkey().to_string(),"trader":trader.pubkey().to_string(),"lp":lp.pubkey().to_string(),"transactions":transactions,"accounts":accounts})).unwrap()).unwrap();
}

/* ---------- token pairs ---------- */

use fix_amm::{Pair, PairCreated, PairReceipt};

/// Two token mints in canonical order: `a` holds the smaller address.
#[derive(Clone, Copy)]
struct Duo { a: Asset, b: Asset }
impl Duo {
    fn new(x: Asset, y: Asset) -> Self { if x.mint < y.mint { Self { a: x, b: y } } else { Self { a: y, b: x } } }
    fn pair(&self) -> Pubkey { pda(&[b"pair", self.a.mint.as_ref(), self.b.mint.as_ref()]) }
    fn vault_a(&self) -> Pubkey { pda(&[b"vault", self.pair().as_ref(), self.a.mint.as_ref()]) }
    fn vault_b(&self) -> Pubkey { pda(&[b"vault", self.pair().as_ref(), self.b.mint.as_ref()]) }
    fn position(&self, owner: &Pubkey) -> Pubkey { pda(&[b"position", self.pair().as_ref(), owner.as_ref()]) }
}
fn pair_receipts(meta: &TransactionMetadata) -> Vec<PairReceipt> {
    events(meta).iter().filter(|b| b.starts_with(PairReceipt::DISCRIMINATOR)).map(|b| PairReceipt::deserialize(&mut &b[8..]).unwrap()).collect()
}
fn pair_receipt(meta: &TransactionMetadata) -> PairReceipt { let mut all = pair_receipts(meta); assert_eq!(all.len(), 1, "{:?}", meta.logs); all.remove(0) }
fn pair_receipt_json(r: &PairReceipt) -> Value {
    json!({"action":r.action,"amountA":r.amount_a.to_string(),"amountB":r.amount_b.to_string(),"protocol":r.protocol_fee.to_string(),"lp":r.lp_fee.to_string(),
        "shares":r.shares.to_string(),"totalShares":r.total_shares.to_string(),"reserveA":r.reserve_a.to_string(),"reserveB":r.reserve_b.to_string(),
        "priceBefore":r.price_before.to_string(),"priceAfter":r.price_after.to_string(),"priceImpactBps":r.price_impact_bps.to_string()})
}
/// The pair swap model, apart from the program's math: `x` is the input reserve and `y` the output reserve.
fn pair_model(exact_in: bool, amount: u64, x: u64, y: u64) -> (u64, u64, u64, u64) {
    let (x1, y1) = (x as u128, y as u128);
    if exact_in {
        let (t, p, l) = fee(amount); let out = (y1 * (amount - t) as u128 / (x1 + (amount - t) as u128)) as u64; (amount, out, p, l)
    } else {
        let net = (x1 * amount as u128).div_ceil(y1 - amount as u128) as u64; let g = gross_for(net); let (_, p, l) = fee(g); (g, amount, p, l)
    }
}
impl Harness {
    fn create_pair_ix(&self, d: Duo, creator: Pubkey, ta: Pubkey, tb: Pubkey, amount_a: u64, amount_b: u64) -> Instruction {
        let market = |m: &Pubkey| Pubkey::find_program_address(&[b"market", m.as_ref()], &LAUNCHPAD_ID).0;
        ix(instruction::CreatePair { amount_a, amount_b, expected_config_revision: 0 }, accounts::CreatePair {
            creator, config: self.config, mint_a: d.a.mint, mint_b: d.b.mint, pair: d.pair(), vault_a: d.vault_a(), vault_b: d.vault_b(), creator_token_a: ta, creator_token_b: tb,
            position: d.position(&creator), launchpad_market_a: market(&d.a.mint), launchpad_market_b: market(&d.b.mint), token_program_a: d.a.program, token_program_b: d.b.program,
            system_program: solana_sdk::system_program::ID, event_authority: event_authority(), program: ID })
    }
    fn pair_swap_ix(&self, d: Duo, a_to_b: bool, exact_in: bool, amount: u64, bound: u64, trader: Pubkey, ta: Pubkey, tb: Pubkey) -> Instruction {
        let accs = accounts::PairSwap { trader, pair: d.pair(), mint_a: d.a.mint, mint_b: d.b.mint, vault_a: d.vault_a(), vault_b: d.vault_b(), trader_token_a: ta, trader_token_b: tb,
            token_program_a: d.a.program, token_program_b: d.b.program, event_authority: event_authority(), program: ID };
        if exact_in { ix(instruction::SwapPairExactIn { a_to_b, amount_in: amount, min_out: bound }, accs) } else { ix(instruction::SwapPairExactOut { a_to_b, amount_out: amount, max_in: bound }, accs) }
    }
    fn pair_add_ix(&self, d: Duo, owner: Pubkey, ta: Pubkey, tb: Pubkey, shares: u64, max_a: u64, max_b: u64) -> Instruction {
        ix(instruction::AddPairLiquidity { shares, max_a, max_b }, accounts::AddPairLiquidity { owner, pair: d.pair(), mint_a: d.a.mint, mint_b: d.b.mint, vault_a: d.vault_a(), vault_b: d.vault_b(),
            owner_token_a: ta, owner_token_b: tb, position: d.position(&owner), token_program_a: d.a.program, token_program_b: d.b.program, system_program: solana_sdk::system_program::ID, event_authority: event_authority(), program: ID })
    }
    fn pair_remove_ix(&self, d: Duo, owner: Pubkey, ta: Pubkey, tb: Pubkey, shares: u64, min_a: u64, min_b: u64) -> Instruction {
        ix(instruction::RemovePairLiquidity { shares, min_a, min_b }, accounts::RemovePairLiquidity { owner, pair: d.pair(), mint_a: d.a.mint, mint_b: d.b.mint, vault_a: d.vault_a(), vault_b: d.vault_b(),
            owner_token_a: ta, owner_token_b: tb, position: d.position(&owner), token_program_a: d.a.program, token_program_b: d.b.program, event_authority: event_authority(), program: ID })
    }
    fn pair(&self, d: Duo) -> Pair { self.decode(d.pair()) }
    /// Each vault holds its reserve and the protocol's booked part; shares are the positions' sum.
    fn assert_pair_books(&self, d: Duo, owners: &[Pubkey]) {
        let p = self.pair(d);
        assert_eq!(self.tokens(d.vault_a()), p.reserve_a + p.protocol_a, "vault A"); assert_eq!(self.tokens(d.vault_b()), p.reserve_b + p.protocol_b, "vault B");
        let held: u64 = owners.iter().filter_map(|o| self.svm.get_account(&d.position(o)).map(|_| self.decode::<Position>(d.position(o)).shares)).sum();
        assert_eq!(held, p.shares, "shares");
    }
    /// A wallet holding both tokens.
    fn duo_wallet(&mut self, d: Duo, a: u64, b: u64) -> (Keypair, Pubkey, Pubkey) {
        let w = self.wallet(10 * SOL); let ta = self.holder(d.a, w.pubkey(), a); let tb = self.holder(d.b, w.pubkey(), b); (w, ta, tb)
    }
}

#[test]
fn pairs_open_trade_both_ways_and_book_the_protocol_part_in_the_input_token() {
    let mut vectors = vec![];
    let mut h = Harness::new();
    let x = h.mint(Kind::Spl); let y = h.mint(Kind::Token2022Metadata); let d = Duo::new(x, y);
    let (creator, ca, cb) = h.duo_wallet(d, 9_000_000 * TOKEN, 9_000_000 * TOKEN);
    let (trader, ta, tb) = h.duo_wallet(d, 3_000_000 * TOKEN, 3_000_000 * TOKEN);
    let (amount_a, amount_b) = (2_000_000 * TOKEN, 5_000_000 * TOKEN);
    // Creation and a swap in one transaction fail together.
    let create = h.create_pair_ix(d, creator.pubkey(), ca, cb, amount_a, amount_b); let swap = h.pair_swap_ix(d, true, true, 1_000 * TOKEN, 1, creator.pubkey(), ca, cb);
    assert!(h.send(vec![create, swap], &[&creator]).unwrap_err().contains("SwapNotOpen")); assert!(h.svm.get_account(&d.pair()).is_none());
    let created = h.send(vec![h.create_pair_ix(d, creator.pubkey(), ca, cb, amount_a, amount_b)], &[&creator]).unwrap();
    let ev = events(&created).into_iter().find(|b| b.starts_with(PairCreated::DISCRIMINATOR)).map(|b| PairCreated::deserialize(&mut &b[8..]).unwrap()).unwrap();
    assert_eq!((ev.mint_a, ev.mint_b, ev.token_program_a, ev.token_program_b, ev.created_slot), (d.a.mint, d.b.mint, d.a.program, d.b.program, h.slot()));
    let r = pair_receipt(&created); let shares = isqrt(amount_a as u128 * amount_b as u128);
    assert_eq!((r.action, r.amount_a, r.amount_b, r.shares, r.total_shares, r.price_before), (0, amount_a, amount_b, shares, shares, 0));
    assert_eq!(r.price_after, ((amount_b as u128) << 64) / amount_a as u128);
    h.assert_pair_books(d, &[creator.pubkey()]);
    // The seeding slot takes no swap in either direction, from anyone.
    for (a_to_b, exact_in) in [(true, true), (true, false), (false, true), (false, false)] {
        let swap = h.pair_swap_ix(d, a_to_b, exact_in, 1_000 * TOKEN, if exact_in { 1 } else { u64::MAX }, trader.pubkey(), ta, tb);
        assert!(h.send_same_slot(vec![swap], &[&trader]).unwrap_err().contains("SwapNotOpen"));
    }
    // Both directions, exact in and exact out, equal the model; the trader's balances equal the receipt.
    let plan: [(bool, bool, u64); 8] = [(true, true, 40_000 * TOKEN + 7), (false, true, 90_001 * TOKEN), (true, false, 33_333 * TOKEN), (false, false, 12_345 * TOKEN + 9),
        (true, true, 3), (false, false, 1), (true, true, 600_000 * TOKEN), (false, true, 1_500_000 * TOKEN)];
    for (a_to_b, exact_in, amount) in plan {
        let before = h.pair(d);
        let (x, y) = if a_to_b { (before.reserve_a, before.reserve_b) } else { (before.reserve_b, before.reserve_a) };
        let (gross, out, protocol, lp) = pair_model(exact_in, amount, x, y);
        if out == 0 { assert!(h.send(vec![h.pair_swap_ix(d, a_to_b, exact_in, amount, 0, trader.pubkey(), ta, tb)], &[&trader]).unwrap_err().contains("SlippageExceeded")); continue; }
        let bound = if exact_in { out } else { gross };
        let tighter = h.pair_swap_ix(d, a_to_b, exact_in, amount, if exact_in { bound + 1 } else { bound - 1 }, trader.pubkey(), ta, tb);
        let refused = h.send(vec![tighter], &[&trader]).unwrap_err(); assert!(refused.contains("SlippageExceeded") || (bound == 1 && refused.contains("ZeroAmount")), "{refused}");
        let (a0, b0) = (h.tokens(ta), h.tokens(tb));
        let r = pair_receipt(&h.send(vec![h.pair_swap_ix(d, a_to_b, exact_in, amount, bound, trader.pubkey(), ta, tb)], &[&trader]).unwrap());
        let after = h.pair(d);
        let (paid, got) = if a_to_b { (a0 - h.tokens(ta), h.tokens(tb) - b0) } else { (b0 - h.tokens(tb), h.tokens(ta) - a0) };
        assert_eq!((paid, got, r.protocol_fee, r.lp_fee), (gross, out, protocol, lp), "{a_to_b} {exact_in} {amount}");
        assert_eq!(r.action, if a_to_b { 1 } else { 2 });
        assert_eq!((r.amount_a, r.amount_b), if a_to_b { (gross, out) } else { (out, gross) });
        let (xa, ya) = if a_to_b { (after.reserve_a, after.reserve_b) } else { (after.reserve_b, after.reserve_a) };
        assert_eq!((xa, ya), (x + gross - protocol, y - out));
        assert_eq!(if a_to_b { after.protocol_a - before.protocol_a } else { after.protocol_b - before.protocol_b }, protocol, "protocol part booked in the input token");
        assert!(after.reserve_a as u128 * after.reserve_b as u128 >= before.reserve_a as u128 * before.reserve_b as u128);
        assert_eq!((r.reserve_a, r.reserve_b, r.price_after), (after.reserve_a, after.reserve_b, ((after.reserve_b as u128) << 64) / after.reserve_a as u128));
        h.assert_pair_books(d, &[creator.pubkey()]);
        vectors.push(json!({"aToB":a_to_b,"exactIn":exact_in,"amount":amount.to_string(),"before":{"reserveA":before.reserve_a.to_string(),"reserveB":before.reserve_b.to_string(),"shares":before.shares.to_string()},"receipt":pair_receipt_json(&r)}));
    }
    // Liquidity at the pair's ratio, rounded toward the pair; the protocol's booked part never leaves with it.
    let (lp, la, lb) = h.duo_wallet(d, 5_000_000 * TOKEN, 9_000_000 * TOKEN);
    let p = h.pair(d); let shares = p.shares / 4 + 3;
    let need_a = (shares as u128 * p.reserve_a as u128).div_ceil(p.shares as u128) as u64; let need_b = (shares as u128 * p.reserve_b as u128).div_ceil(p.shares as u128) as u64;
    assert!(h.send(vec![h.pair_add_ix(d, lp.pubkey(), la, lb, shares, need_a - 1, u64::MAX)], &[&lp]).unwrap_err().contains("SlippageExceeded"));
    let r = pair_receipt(&h.send(vec![h.pair_add_ix(d, lp.pubkey(), la, lb, shares, need_a, need_b)], &[&lp]).unwrap());
    assert_eq!((r.action, r.amount_a, r.amount_b, r.shares), (3, need_a, need_b, shares));
    h.assert_pair_books(d, &[creator.pubkey(), lp.pubkey()]);
    for (owner, ta, tb) in [(&lp, la, lb), (&creator, ca, cb)] {
        let held = h.decode::<Position>(d.position(&owner.pubkey())).shares; let p = h.pair(d);
        let (out_a, out_b) = ((held as u128 * p.reserve_a as u128 / p.shares as u128) as u64, (held as u128 * p.reserve_b as u128 / p.shares as u128) as u64);
        let (a0, b0) = (h.tokens(ta), h.tokens(tb));
        let r = pair_receipt(&h.send(vec![h.pair_remove_ix(d, owner.pubkey(), ta, tb, held, out_a, out_b)], &[owner]).unwrap());
        assert_eq!((r.action, r.amount_a, r.amount_b, h.tokens(ta) - a0, h.tokens(tb) - b0), (4, out_a, out_b, out_a, out_b));
    }
    let p = h.pair(d);
    assert_eq!((p.reserve_a, p.reserve_b, p.shares), (0, 0, 0), "the last shares take both reserves");
    assert_eq!((h.tokens(d.vault_a()), h.tokens(d.vault_b())), (p.protocol_a, p.protocol_b), "only the protocol's booked part stays in the vaults");
    assert!(p.protocol_a > 0 && p.protocol_b > 0);
    assert!(h.send(vec![h.pair_swap_ix(d, true, true, 1_000, 1, trader.pubkey(), ta, tb)], &[&trader]).unwrap_err().contains("InsufficientLiquidity"));
    // Seeding the empty pair again closes that slot to swaps.
    let seed = isqrt(1_000 * TOKEN as u128 * 3_000 * TOKEN as u128);
    let r = pair_receipt(&h.send(vec![h.pair_add_ix(d, lp.pubkey(), la, lb, seed, 1_000 * TOKEN, 3_000 * TOKEN)], &[&lp]).unwrap());
    assert_eq!((r.shares, r.reserve_a, r.reserve_b, r.price_before), (seed, 1_000 * TOKEN, 3_000 * TOKEN, 0));
    let swap = h.pair_swap_ix(d, false, true, 10 * TOKEN, 1, trader.pubkey(), ta, tb);
    assert!(h.send_same_slot(vec![swap.clone()], &[&trader]).unwrap_err().contains("SwapNotOpen"));
    h.send(vec![swap], &[&trader]).unwrap();
    h.assert_pair_books(d, &[creator.pubkey(), lp.pubkey()]);
    // Every executed pair transaction, for the SDK decoder and indexer regressions.
    let transactions: Vec<Value> = h.recorded.iter().filter(|tx| tx["innerInstructions"].as_array().unwrap().iter().flat_map(|g| g["instructions"].as_array().unwrap().iter())
        .any(|entry| STANDARD.decode(entry["dataBase64"].as_str().unwrap()).unwrap().strip_prefix(anchor_lang::event::EVENT_IX_TAG_LE).is_some_and(|bytes| bytes.starts_with(PairReceipt::DISCRIMINATOR)))).cloned().collect();
    let accounts: Vec<Value> = [d.pair(), d.position(&creator.pubkey()), d.position(&lp.pubkey())].iter().filter_map(|k| h.svm.get_account(k).map(|acc| json!({"address":k.to_string(),"owner":acc.owner.to_string(),"dataBase64":STANDARD.encode(&acc.data)}))).collect();
    std::fs::write(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/amm-pair-event-vectors.json"), serde_json::to_string_pretty(&json!({"format":"fix-amm-pair-litesvm-v1","programId":ID.to_string(),
        "pair":d.pair().to_string(),"mintA":d.a.mint.to_string(),"mintB":d.b.mint.to_string(),"transactions":transactions,"accounts":accounts})).unwrap()).unwrap();
    let file = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/amm-pair-vectors.json");
    std::fs::write(file, serde_json::to_string_pretty(&json!({"source":"LiteSVM executed fix_amm SBF pair receipts","terms":{"feeBps":TERMS.fee_bps,"protocolShareBps":TERMS.protocol_share_bps},"vectors":vectors})).unwrap()).unwrap();
}

#[test]
fn pairs_refuse_by_name_and_measure_their_units() {
    let mut h = Harness::new();
    let x = h.mint(Kind::Spl); let y = h.mint(Kind::Token2022Metadata); let d = Duo::new(x, y);
    let (creator, ca, cb) = h.duo_wallet(d, 5_000_000 * TOKEN, 5_000_000 * TOKEN);
    // Mints out of order, or the same mint twice, are refused.
    let reversed = Duo { a: d.b, b: d.a };
    assert!(h.send(vec![h.create_pair_ix(reversed, creator.pubkey(), cb, ca, TOKEN, TOKEN)], &[&creator]).unwrap_err().contains("PairOrder"));
    // The same mint twice fails on its second vault before the order rule; either way nothing is created.
    let same = Duo { a: d.a, b: d.a };
    assert!(h.send(vec![h.create_pair_ix(same, creator.pubkey(), ca, ca, TOKEN, TOKEN)], &[&creator]).is_err());
    assert!(h.svm.get_account(&same.pair()).is_none());
    // A launchpad coin or a transfer-fee mint on either side is refused.
    let coin = h.mint(Kind::Token2022Metadata);
    h.svm.set_account(Pubkey::find_program_address(&[b"market", coin.mint.as_ref()], &LAUNCHPAD_ID).0, Account { lamports: 2_000_000, data: vec![1; 222], owner: LAUNCHPAD_ID, executable: false, rent_epoch: 0 }).unwrap();
    let fee_mint = h.mint(Kind::Token2022(ExtensionType::TransferFeeConfig));
    for (other, error) in [(coin, "LaunchpadCoin"), (fee_mint, "UnsupportedMint")] {
        let e = Duo::new(x, other); let (w, ta, tb) = h.duo_wallet(e, 1_000 * TOKEN, 1_000 * TOKEN);
        assert!(h.send(vec![h.create_pair_ix(e, w.pubkey(), ta, tb, 1_000 * TOKEN, 1_000 * TOKEN)], &[&w]).unwrap_err().contains(error), "{error}");
    }
    // Units per pair action.
    let cu = |m: &TransactionMetadata| m.compute_units_consumed;
    let created = cu(&h.send(vec![h.create_pair_ix(d, creator.pubkey(), ca, cb, 2_000_000 * TOKEN, 3_000_000 * TOKEN)], &[&creator]).unwrap());
    let (t, ta, tb) = h.duo_wallet(d, 500_000 * TOKEN, 500_000 * TOKEN);
    let mut swap = 0;
    for (a_to_b, exact_in, bound) in [(true, true, 1), (true, false, u64::MAX), (false, true, 1), (false, false, u64::MAX)] {
        swap = swap.max(cu(&h.send(vec![h.pair_swap_ix(d, a_to_b, exact_in, 10_000 * TOKEN, bound, t.pubkey(), ta, tb)], &[&t]).unwrap()));
    }
    let add_new = cu(&h.send(vec![h.pair_add_ix(d, t.pubkey(), ta, tb, 1_000_000, u64::MAX, u64::MAX)], &[&t]).unwrap());
    let add = cu(&h.send(vec![h.pair_add_ix(d, t.pubkey(), ta, tb, 1_000_000, u64::MAX, u64::MAX)], &[&t]).unwrap());
    let remove = cu(&h.send(vec![h.pair_remove_ix(d, t.pubkey(), ta, tb, 1_500_000, 0, 0)], &[&t]).unwrap());
    let report = json!({"source":"LiteSVM 0.6.1, compiled fix_amm SBF, one SPL Token and one Token-2022 mint","createPair":created,"pairSwap":swap,"addPairLiquidityNewPosition":add_new,"addPairLiquidity":add,"removePairLiquidity":remove});
    std::fs::write(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/amm-pair-compute-units.json"), serde_json::to_string_pretty(&report).unwrap()).unwrap();
    println!("Pair compute units: {report}");
    for (field, ceiling) in [("createPair", 130_000u64), ("pairSwap", 70_000), ("addPairLiquidityNewPosition", 70_000), ("addPairLiquidity", 60_000), ("removePairLiquidity", 60_000)] {
        assert!(report[field].as_u64().unwrap() <= ceiling, "{field} exceeds {ceiling}: {report}");
    }
}
