use anchor_lang::{AccountDeserialize, AnchorDeserialize, Discriminator, InstructionData, ToAccountMetas};
use base64::{engine::general_purpose::STANDARD, Engine};
use fix_launchpad::{accounts, instruction, math, CoinCreated, Market, Protocol, Receipt, Terms, ID, PROTOCOL_SHARDS};
use litesvm::{types::TransactionMetadata, LiteSVM};
use serde_json::{json, Value};
use solana_sdk::{account::Account, bpf_loader_upgradeable::{self, UpgradeableLoaderState}, clock::Clock, compute_budget::ComputeBudgetInstruction, instruction::{AccountMeta, Instruction}, pubkey::Pubkey, signature::{Keypair, Signer}, transaction::Transaction};
use anchor_lang::solana_program::{program_option::COption, program_pack::Pack};
use spl_token_2022::{extension::{BaseStateWithExtensions, StateWithExtensions}, state::{Account as TokenAccount, Mint}};

// Fixture values only. These are deliberately small and are not a launch recommendation.
fn terms() -> Terms { Terms { total_supply: 1_000_000_000, decimals: 6,
    virtual_sol: 100_000_000, virtual_token: 1_000_000_000, curve_tokens: 600_000_000,
    curve_fee_bps: 300, pool_fee_bps: 100,
    protocol_share_bps: 5_000, creator_share_bps: 3_000, pool_share_bps: 2_000 } }
fn pda(seeds: &[&[u8]]) -> Pubkey { Pubkey::find_program_address(seeds, &ID).0 }
fn fixture_mint(index:usize)->Keypair {
    let fixtures:Value=serde_json::from_str(include_str!("fixtures/fix-mints.json")).unwrap();
    let row=&fixtures["mints"][index];
    let secret:Vec<u8>=row["secretKey"].as_array().unwrap().iter().map(|value|value.as_u64().unwrap() as u8).collect();
    let keypair=Keypair::from_bytes(&secret).unwrap();
    assert_eq!(keypair.pubkey().to_string(),row["address"].as_str().unwrap());
    assert!(keypair.pubkey().to_string().ends_with("fix"));keypair
}
fn ix(data: impl InstructionData, accs: impl ToAccountMetas) -> Instruction { Instruction { program_id: ID, data: data.data(), accounts: accs.to_account_metas(None) } }
fn decode<T: AccountDeserialize>(svm: &LiteSVM, key: Pubkey) -> T { let a = svm.get_account(&key).unwrap(); T::try_deserialize(&mut a.data.as_slice()).unwrap() }
fn event_authority() -> Pubkey { pda(&[b"__event_authority"]) }
/// The SDK's default shard for a market; the program accepts any created shard.
fn protocol_shard(market: &Pubkey) -> u8 { market.to_bytes()[0] % PROTOCOL_SHARDS }
/// Event bytes (discriminator and payload) from self-CPI inner instructions, in execution order.
fn events(meta: &TransactionMetadata) -> Vec<Vec<u8>> {
    meta.inner_instructions.iter().flatten().filter_map(|inner| inner.instruction.data.strip_prefix(anchor_lang::event::EVENT_IX_TAG_LE)).map(<[u8]>::to_vec).collect()
}
fn receipts(meta: &TransactionMetadata) -> Vec<Receipt> {
    events(meta).iter().filter(|b| b.starts_with(Receipt::DISCRIMINATOR)).map(|b| Receipt::deserialize(&mut &b[8..]).unwrap()).collect()
}
fn receipt(meta: &TransactionMetadata, action: u8) -> Receipt {
    receipts(meta).into_iter().find(|r| r.action == action)
        .unwrap_or_else(|| panic!("Missing receipt action {action}: {:?}", meta.logs))
}

struct Harness { svm: LiteSVM, payer: Keypair, creator: Keypair, config: Pubkey, protocol: Pubkey, mint: Keypair, market: Pubkey, vault: Pubkey, vectors: Vec<Value>, recorded: Vec<Value>, lighthouse: Option<Pubkey> }
impl Harness {
    fn new() -> Self {
        Self::with_prefund(false)
    }
    fn with_prefund(prefund:bool) -> Self {
        Self::with_fixture(terms(),prefund)
    }
    fn with_terms(fixture:Terms) -> Self { Self::with_fixture(fixture,false) }
    fn initialized(fixture:Terms) -> Self {
        Self::initialized_artifact(fixture,"fix_launchpad.so")
    }
    fn initialized_artifact(fixture:Terms,artifact_name:&str) -> Self {
        let mut svm = LiteSVM::new();
        let artifact = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/deploy").join(artifact_name);
        assert!(artifact.exists(), "Build SBF first: cargo build-sbf --manifest-path programs/fix-launchpad/Cargo.toml --arch v0");
        let elf = std::fs::read(artifact).unwrap();
        let payer = Keypair::new(); let creator = Keypair::new(); let mint = fixture_mint(0);
        svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap(); svm.airdrop(&creator.pubkey(), 10_000_000_000).unwrap();
        let program_data=Pubkey::find_program_address(&[ID.as_ref()],&bpf_loader_upgradeable::ID).0;
        let mut program_bytes=bincode::serialize(&UpgradeableLoaderState::ProgramData { slot:0, upgrade_authority_address:Some(creator.pubkey()) }).unwrap(); program_bytes.resize(UpgradeableLoaderState::size_of_programdata_metadata(),0); program_bytes.extend_from_slice(&elf);
        let rent:solana_sdk::rent::Rent=svm.get_sysvar(); svm.set_account(program_data,Account { lamports:rent.minimum_balance(program_bytes.len()),data:program_bytes,owner:bpf_loader_upgradeable::ID,executable:false,rent_epoch:0 }).unwrap();
        let data=bincode::serialize(&UpgradeableLoaderState::Program { programdata_address:program_data }).unwrap(); svm.set_account(ID,Account { lamports:rent.minimum_balance(data.len()),data,owner:bpf_loader_upgradeable::ID,executable:true,rent_epoch:0 }).unwrap();
        let config = pda(&[b"config"]); let market = pda(&[b"market", mint.pubkey().as_ref()]); let shard = protocol_shard(&market); let protocol = pda(&[b"protocol", &[shard]]); let vault = pda(&[b"vault", market.as_ref()]);
        let mut h = Self { svm, payer, creator, config, protocol, mint, market, vault, vectors: vec![], recorded: vec![], lighthouse: None }; h.time(1_000);
        let init = ix(instruction::InitializeConfig { terms: fixture }, accounts::InitializeConfig { admin: h.creator.pubkey(), program:ID, program_data, config, system_program: solana_sdk::system_program::ID });
        let creator_signer=h.creator.insecure_clone();
        h.send(vec![init, h.shard_ix(shard)], &[&creator_signer]).unwrap();
        h
    }
    fn with_fixture(fixture:Terms,prefund:bool) -> Self {
        let mut h=Self::initialized(fixture);let creator_signer=h.creator.insecure_clone();let mint_signer=h.mint.insecure_clone();
        if prefund {
            let transfers=vec![solana_sdk::system_instruction::transfer(&h.creator.pubkey(),&h.mint.pubkey(),1_000_000),solana_sdk::system_instruction::transfer(&h.creator.pubkey(),&h.vault,1_000_000)];
            h.send(transfers,&[&creator_signer]).unwrap();
        }
        let create=h.create_ix();h.send(vec![create],&[&creator_signer,&mint_signer]).unwrap();h
    }
    fn shard_ix(&self, shard: u8) -> Instruction {
        ix(instruction::CreateProtocolShard { shard }, accounts::CreateProtocolShard { payer: self.payer.pubkey(), protocol: pda(&[b"protocol", &[shard]]), system_program: solana_sdk::system_program::ID })
    }
    fn create_ix(&self) -> Instruction {
        ix(instruction::CreateCoin {name:"Fixture Coin".into(),symbol:"FIXT".into(),uri:"https://example.invalid/fixture.json".into(),expected_config_revision:0},accounts::CreateCoin {creator:self.creator.pubkey(),config:self.config,mint:self.mint.pubkey(),market:self.market,vault:self.vault,token_program:spl_token_2022::ID,system_program:solana_sdk::system_program::ID,event_authority:event_authority(),program:ID})
    }
    fn advance_slot(&mut self) {let mut clock:Clock=self.svm.get_sysvar();clock.slot=clock.slot.checked_add(1).unwrap();self.svm.set_sysvar(&clock);}
    fn send(&mut self,ixs:Vec<Instruction>,signers:&[&Keypair])->Result<TransactionMetadata,String> {
        self.advance_slot();self.send_same_slot(ixs,signers)
    }
    fn send_same_slot(&mut self, mut ixs: Vec<Instruction>, signers: &[&Keypair]) -> Result<TransactionMetadata, String> {
        self.svm.expire_blockhash(); ixs.insert(0, ComputeBudgetInstruction::set_compute_unit_limit(1_400_000));
        if let Some(program_id) = self.lighthouse {
            // Actual Lighthouse AssertAccountInfo: plaintext lamports == config rent.
            let guard_account = self.config;
            let mut data = vec![5, 1, 0]; data.extend_from_slice(&self.balance(guard_account).to_le_bytes()); data.push(0);
            ixs.push(Instruction { program_id, accounts: vec![solana_sdk::instruction::AccountMeta::new_readonly(guard_account, false)], data });
        }
        let mut all: Vec<&Keypair> = vec![&self.payer]; all.extend_from_slice(signers);
        let tx = Transaction::new_signed_with_payer(&ixs, Some(&self.payer.pubkey()), &all, self.svm.latest_blockhash());
        let transaction_bytes = STANDARD.encode(bincode::serialize(&tx).unwrap());
        let meta = self.svm.send_transaction(tx).map_err(|e| format!("{:?}\n{:?}", e.err, e.meta.logs))?;
        let clock: Clock = self.svm.get_sysvar();
        let inner: Vec<Value> = meta.inner_instructions.iter().enumerate().filter(|(_, instructions)| !instructions.is_empty()).map(|(index, instructions)| json!({
            "index": index,
            "instructions": instructions.iter().map(|entry| json!({
                "programIdIndex": entry.instruction.program_id_index,
                "accounts": entry.instruction.accounts,
                "dataBase64": STANDARD.encode(&entry.instruction.data),
                "stackHeight": entry.stack_height,
            })).collect::<Vec<_>>(),
        })).collect();
        self.recorded.push(json!({ "signature": meta.signature.to_string(), "slot": clock.slot, "unixTimestamp": clock.unix_timestamp,
            "transactionBase64": transaction_bytes, "logs": meta.logs, "innerInstructions": inner }));
        Ok(meta)
    }
    fn time(&mut self, timestamp: i64) { let mut c: Clock = self.svm.get_sysvar(); c.unix_timestamp = timestamp; self.svm.set_sysvar(&c); }
    fn wallet(&mut self) -> Keypair { let w=Keypair::new(); self.svm.airdrop(&w.pubkey(), 10_000_000_000).unwrap(); w }
    fn m(&self) -> Market { decode(&self.svm, self.market) }
    fn assert_sol_accounting(&self) { let m=self.m();let a=self.svm.get_account(&self.market).unwrap();let rent:solana_sdk::rent::Rent=self.svm.get_sysvar();let liabilities=m.real_sol+m.creator_fees;assert_eq!(a.lamports,rent.minimum_balance(a.data.len())+liabilities,"market reserve/fee/liability segregation");let p:Protocol=decode(&self.svm,self.protocol);let pa=self.svm.get_account(&self.protocol).unwrap();assert_eq!(pa.lamports,rent.minimum_balance(pa.data.len())+p.accrued); }
    fn balance(&self, key: Pubkey) -> u64 { self.svm.get_balance(&key).unwrap_or(0) }
    fn token_balance(&self, key: Pubkey) -> u64 { StateWithExtensions::<TokenAccount>::unpack(&self.svm.get_account(&key).unwrap().data).unwrap().base.amount }
    fn token_account(&mut self, owner: Pubkey) -> Pubkey {
        let a=Keypair::new(); let rent: solana_sdk::rent::Rent=self.svm.get_sysvar();
        let create=solana_sdk::system_instruction::create_account(&self.payer.pubkey(), &a.pubkey(), rent.minimum_balance(TokenAccount::LEN), TokenAccount::LEN as u64, &spl_token_2022::ID);
        let init=spl_token_2022::instruction::initialize_account3(&spl_token_2022::ID,&a.pubkey(),&self.mint.pubkey(),&owner).unwrap();
        self.send(vec![create,init], &[&a]).unwrap(); a.pubkey()
    }

    fn trade_ix(&self,data:impl InstructionData,who:Pubkey,token:Pubkey) -> Instruction {
        ix(data,accounts::Trade { trader:who,market:self.market,mint:self.mint.pubkey(),vault:self.vault,trader_token:token,protocol:self.protocol,token_program:spl_token_2022::ID,system_program:solana_sdk::system_program::ID,event_authority:event_authority(),program:ID })
    }
    fn trade(&mut self,mode:&str,amount:u64,bound:u64,who:&Keypair,token:Pubkey) -> Result<TransactionMetadata,String> {
        self.advance_slot();
        let before=self.snapshot();
        let ins=match mode { "buyExactIn"=>self.trade_ix(instruction::BuyExactIn { gross:amount,min_tokens:bound },who.pubkey(),token), "buyExactOut"=>self.trade_ix(instruction::BuyExactOut { tokens:amount,max_gross:bound },who.pubkey(),token), "sellExactIn"=>self.trade_ix(instruction::SellExactIn { tokens:amount,min_sol:bound },who.pubkey(),token), "sellExactOut"=>self.trade_ix(instruction::SellExactOut { sol:amount,max_tokens:bound },who.pubkey(),token), _=>panic!("unknown mode") };
        let meta=self.send_same_slot(vec![ins],&[who])?;
        self.assert_sol_accounting();
        let action=if mode.starts_with("buy") { 1 } else { 2 };
        let r=receipt(&meta,action);
        self.vectors.push(json!({"mode":mode,"amount":amount.to_string(),"before":before,"after":self.snapshot(),"receipt":receipt_json(&r)})); Ok(meta)
    }
    fn snapshot(&self) -> Value { let m=self.m();let clock:Clock=self.svm.get_sysvar(); json!({"createdSlot":m.created_slot.to_string(),"slot":clock.slot.to_string(),"phase":match m.phase {0=>"curve",1=>"pool",_=>panic!("invalid phase")},"virtualSol":m.virtual_sol.to_string(),"virtualToken":m.virtual_token.to_string(),"curveRemaining":m.curve_remaining.to_string(),"realSol":m.real_sol.to_string(),"poolTokenNumerator":m.pool_token_num.to_string(),"poolTokenDenominator":m.pool_token_den.to_string(),"physicalTokens":self.token_balance(self.vault).to_string()}) }

    /// Loads the TEST-ONLY extension program and registers it with `share_bps` of the protocol part.
    fn extension(&mut self, share_bps: u16) -> Extension {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/deploy/fix_test_extension.so");
        assert!(path.exists(), "Build the test extension: cargo-build-sbf --manifest-path programs/fix-test-extension/Cargo.toml --arch v0");
        let program = Pubkey::new_from_array([0x7e; 32]);
        if self.svm.get_account(&program).is_none() { self.svm.add_program_from_file(program, path).unwrap(); }
        let recipient = Keypair::new().pubkey(); self.svm.airdrop(&recipient, 1_000_000_000).unwrap();
        let ext = Extension { program, entry: pda(&[b"extension", program.as_ref()]), authority: Pubkey::find_program_address(&[b"fix-extension"], &program).0, recipient };
        let admin = self.creator.insecure_clone();
        self.send(vec![ext.register_ix(admin.pubkey(), self.config, share_bps)], &[&admin]).unwrap();
        ext
    }
    /// The core trade wrapped in the extension's instruction, with the three routing accounts appended.
    fn routed(&self, ext: &Extension, mode: u8, core: Instruction, recipient: Pubkey) -> Instruction {
        let mut accounts = vec![AccountMeta::new_readonly(ID, false)]; accounts.extend(core.accounts);
        accounts.extend([AccountMeta::new_readonly(ext.entry, false), AccountMeta::new_readonly(ext.authority, false), AccountMeta::new(recipient, false)]);
        let mut data = vec![mode]; data.extend(core.data);
        Instruction { program_id: ext.program, accounts, data }
    }
    fn write_vectors(&self) { let file=std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/chain-vectors.json"); std::fs::write(file,serde_json::to_string_pretty(&json!({"source":"LiteSVM executed SBF receipts","feeTerms":{"curveFeeBps":300,"poolFeeBps":100,"protocolShareBps":5000,"creatorShareBps":3000,"poolShareBps":2000},"vectors":self.vectors})).unwrap()).unwrap(); }

}
struct Extension { program: Pubkey, entry: Pubkey, authority: Pubkey, recipient: Pubkey }
impl Extension {
    fn register_ix(&self, admin: Pubkey, config: Pubkey, share_bps: u16) -> Instruction {
        ix(instruction::RegisterExtension { program_id: self.program, share_bps, recipient: self.recipient }, accounts::RegisterExtension { admin, config, extension: self.entry, system_program: solana_sdk::system_program::ID })
    }
    fn pause_ix(&self, admin: Pubkey, config: Pubkey, paused: bool) -> Instruction {
        ix(instruction::SetExtensionPaused { paused }, accounts::AdminExtension { admin, config, extension: self.entry })
    }
    fn remove_ix(&self, admin: Pubkey, config: Pubkey) -> Instruction {
        ix(instruction::RemoveExtension {}, accounts::RemoveExtension { admin, config, extension: self.entry })
    }
}
fn receipt_json(r:&Receipt)->Value { json!({"extension":r.extension.to_string(),"extensionFee":r.extension_fee.to_string(),"gross":r.gross.to_string(),"protocol":r.protocol_fee.to_string(),"creator":r.creator_fee.to_string(),"pool":r.pool_fee.to_string(),"net":r.net.to_string(),"tokens":r.tokens.to_string(),"refund":r.refund.to_string(),"beforePrice":r.price_before.to_string(),"afterPrice":r.price_after.to_string(),"priceImpactBps":r.price_impact_bps.to_string()}) }

#[test]
fn suffix_remainder_matches_real_base58_encodings() {
    use fix_launchpad::vanity::{has_fix_suffix,FIX_SUFFIX_MODULUS,FIX_SUFFIX_REMAINDER};
    let alphabet=b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
    let digits:Vec<u32>=b"fix".iter().map(|byte|alphabet.iter().position(|candidate|candidate==byte).unwrap() as u32).collect();
    assert_eq!(digits,vec![38,41,55]);
    assert_eq!(FIX_SUFFIX_MODULUS,195_112);
    assert_eq!(digits.iter().fold(0,|value,digit|value*58+digit),130_265);
    assert_eq!(FIX_SUFFIX_REMAINDER,130_265);
    for index in 0..3 {let key=fixture_mint(index).pubkey();assert!(has_fix_suffix(key.as_ref()));assert!(key.to_string().ends_with("fix"));}
    for value in 0..=255 {let key=Pubkey::new_from_array([value;32]);assert_eq!(has_fix_suffix(key.as_ref()),key.to_string().ends_with("fix"));}
    // Exercise canonical base58 encodings with leading-zero bytes and each
    // possible final-three-digit residue, without sampling only valid suffixes.
    for remainder in 0..FIX_SUFFIX_MODULUS {
        let mut bytes=[0u8;32];bytes[28..].copy_from_slice(&remainder.to_be_bytes());
        assert_eq!(has_fix_suffix(&bytes),Pubkey::new_from_array(bytes).to_string().ends_with("fix"));
    }
    assert_eq!(u32::from(fix_launchpad::LaunchError::BuyNotOpen),6009);
    assert_eq!(u32::from(fix_launchpad::LaunchError::InvalidMintSuffix),6010);
}

#[test]
fn creation_refuses_non_fix_mints_without_retaining_any_funds() {
    let mut h=Harness::initialized(terms());let valid=h.mint.insecure_clone();
    loop {h.mint=Keypair::new();if !h.mint.pubkey().to_string().ends_with("fix") {break;}}
    h.market=pda(&[b"market",h.mint.pubkey().as_ref()]);h.vault=pda(&[b"vault",h.market.as_ref()]);
    let creator=h.creator.insecure_clone();let invalid=h.mint.insecure_clone();let balance=h.balance(creator.pubkey());
    let protocol=h.svm.get_account(&h.protocol).unwrap();
    let refused=h.send(vec![h.create_ix()],&[&creator,&invalid]).unwrap_err();
    assert!(refused.contains("InvalidMintSuffix"),"{refused}");assert!(refused.contains("6010"),"{refused}");
    for address in [h.market,h.mint.pubkey(),h.vault] {assert!(h.svm.get_account(&address).is_none());}
    assert_eq!(h.balance(creator.pubkey()),balance);assert_eq!(h.svm.get_account(&h.protocol).unwrap(),protocol);
    h.mint=valid;h.market=pda(&[b"market",h.mint.pubkey().as_ref()]);h.vault=pda(&[b"vault",h.market.as_ref()]);
    let mint=h.mint.insecure_clone();let created=h.send(vec![h.create_ix()],&[&creator,&mint]).unwrap();
    assert_eq!(receipt(&created,0).tokens,0);assert_eq!(h.token_balance(h.vault),terms().total_supply);assert_eq!(h.m().mint,h.mint.pubkey());h.assert_sol_accounting();
    assert!(!created.logs.iter().any(|line|line.contains("MintSuffixCompute:")),"normal builds contain no profiling probes");
}

#[test]
#[ignore="Requires the separately built measure-mint-suffix SBF artifact; run with --include-ignored after building it"]
fn mint_suffix_compute_cost_is_measured_with_probe_overhead_removed() {
    let mut measurements=Vec::new();
    for index in 0..3 {
        let mut h=Harness::initialized_artifact(terms(),"mint-suffix-profile.so");
        h.mint=fixture_mint(index);h.market=pda(&[b"market",h.mint.pubkey().as_ref()]);h.vault=pda(&[b"vault",h.market.as_ref()]);
        let creator=h.creator.insecure_clone();let mint=h.mint.insecure_clone();
        let metadata=h.send(vec![h.create_ix()],&[&creator,&mint]).unwrap();
        let line=metadata.logs.iter().find_map(|line|line.strip_prefix("Program log: MintSuffixCompute: ")).unwrap();
        let parts:Vec<_>=line.split_whitespace().collect();let checked=parts[0].strip_prefix("checked=").unwrap().parse::<u64>().unwrap();let probe=parts[1].strip_prefix("probe=").unwrap().parse::<u64>().unwrap();
        assert!(checked>probe && probe>=100);let net=checked-probe;
        measurements.push(json!({"mint":h.mint.pubkey().to_string(),"checkedRegionCu":checked,"emptyProbeRegionCu":probe,"netCheckCu":net}));
    }
    let report=json!({"source":"LiteSVM executed SBF measure-mint-suffix feature; empty adjacent remaining-CU probes calibrate instrumentation", "measurements":measurements});
    let path=std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/mint-suffix-compute.json");
    std::fs::write(path,serde_json::to_string_pretty(&report).unwrap()).unwrap();
    println!("Mint suffix compute: {}",report);
}

#[test]
fn real_transaction_logs_are_exported_for_sdk_and_indexer_regressions() {
    let mut h=Harness::new();
    let mut selected=vec![(0,"initialize_config"),(1,"create_coin")];
    let lighthouse_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/decoder-fixtures/lighthouse.so");
    if lighthouse_path.exists() {
        let lighthouse = "L2TExMFKdjpN9kozasaurPirfHy9P8sbXoAN1qA3S95".parse::<Pubkey>().unwrap();
        h.svm.add_program_from_file(lighthouse, lighthouse_path).unwrap(); h.lighthouse = Some(lighthouse);
    }
    // A second coin proves standalone creation still has no purchase or fee.
    let extra_mint=fixture_mint(1);let extra_market=pda(&[b"market",extra_mint.pubkey().as_ref()]);
    let extra_vault=pda(&[b"vault",extra_market.as_ref()]);
    let create=ix(instruction::CreateCoin {name:"Empty Fixture Coin".into(),symbol:"EMPTY".into(),uri:"https://example.invalid/empty.json".into(),expected_config_revision:0},accounts::CreateCoin {creator:h.creator.pubkey(),config:h.config,mint:extra_mint.pubkey(),market:extra_market,vault:extra_vault,token_program:spl_token_2022::ID,system_program:solana_sdk::system_program::ID,event_authority:event_authority(),program:ID});
    let creator_signer=h.creator.insecure_clone();h.send(vec![create],&[&creator_signer,&extra_mint]).unwrap();
    selected.push((h.recorded.len()-1,"create_coin"));
    let creator=h.creator.insecure_clone();let creator_token=h.token_account(creator.pubkey());
    h.trade("buyExactIn",2_000_000,1,&creator,creator_token).unwrap();selected.push((h.recorded.len()-1,"creator_buy"));
    let trader=h.wallet();let token=h.token_account(trader.pubkey());
    h.time(1_001);h.trade("buyExactIn",3_000_000,1,&trader,token).unwrap();selected.push((h.recorded.len()-1,"buy"));
    h.time(1_002);h.trade("sellExactIn",1_000_000,0,&trader,token).unwrap();selected.push((h.recorded.len()-1,"sell"));
    // A buy routed by an extension: the core trade runs as an inner instruction and its receipt names the extension.
    let ext=h.extension(2_500);let core=h.trade_ix(instruction::BuyExactIn {gross:2_000_000,min_tokens:1},trader.pubkey(),token);
    h.time(1_003);let routed=h.routed(&ext,0,core,ext.recipient);h.send(vec![routed],&[&trader]).unwrap();selected.push((h.recorded.len()-1,"routed_buy"));
    let mut protocol_fees=0u64;
    let transactions:Vec<Value>=selected.into_iter().map(|(index,name)| {
        let mut item=h.recorded[index].clone();item["name"]=json!(name);let logs=item["logs"].as_array().unwrap();
        assert!(logs.iter().any(|line|line.as_str().unwrap().starts_with("Program log: Instruction: ")));
        assert!(logs.iter().any(|line|line.as_str().unwrap().starts_with(&format!("Program {} consumed ",ID))));
        assert!(!logs.iter().any(|line|line.as_str().unwrap().starts_with("Program data: ")),"events never use the log budget");
        let mut names=vec![];let mut actions=vec![];let mut receipts=vec![];
        let inner=item["innerInstructions"].as_array().unwrap().iter().flat_map(|group|group["instructions"].as_array().unwrap().iter()).map(|entry|STANDARD.decode(entry["dataBase64"].as_str().unwrap()).unwrap()).collect::<Vec<_>>();
        for data in inner {
            if let Some(bytes)=data.strip_prefix(anchor_lang::event::EVENT_IX_TAG_LE) {
                let event=if bytes.starts_with(CoinCreated::DISCRIMINATOR) {"CoinCreated"}
                    else if bytes.starts_with(Receipt::DISCRIMINATOR) {
                        let receipt=Receipt::deserialize(&mut &bytes[8..]).unwrap();protocol_fees+=receipt.protocol_fee;
                        actions.push(receipt.action);receipts.push(receipt_json(&receipt));"Receipt"
                    }else{panic!("Unexpected fixture event discriminator")};
                names.push(event);
            }
        }
        item["expectedEvents"]=json!(names);item["expectedActions"]=json!(actions);item["expectedReceipts"]=json!(receipts);item
    }).collect();
    assert!(transactions.iter().flat_map(|tx|tx["logs"].as_array().unwrap()).any(|line|line.as_str().unwrap().starts_with(&format!("Program {} invoke [2]",spl_token_2022::ID))));
    assert_eq!(transactions[1]["expectedActions"],json!([0]));
    for transaction in &transactions[3..] {assert!(transaction["slot"].as_u64().unwrap()>transactions[1]["slot"].as_u64().unwrap());}
    if let Some(lighthouse)=h.lighthouse {
        let logs=transactions.last().unwrap()["logs"].as_array().unwrap();
        let own_success=logs.iter().position(|line|line==&json!(format!("Program {} success",ID))).unwrap();
        let guard_invoke=logs.iter().position(|line|line==&json!(format!("Program {} invoke [1]",lighthouse))).unwrap();
        assert!(guard_invoke>own_success);assert!(logs.iter().any(|line|line==&json!(format!("Program {} success",lighthouse))));
    }
    let account_keys=[h.config,h.protocol,h.market,h.mint.pubkey(),h.vault,token,creator_token,extra_market,extra_mint.pubkey(),extra_vault,ext.entry];
    let account_rows:Vec<Value>=account_keys.iter().filter_map(|key|h.svm.get_account(key).map(|account|json!({
        "address":key.to_string(),"owner":account.owner.to_string(),"lamports":account.lamports.to_string(),"dataBase64":STANDARD.encode(&account.data),"executable":account.executable,
    }))).collect();
    let clock:Clock=h.svm.get_sysvar();let file=std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/event-log-vectors.json");
    std::fs::write(file,serde_json::to_string_pretty(&json!({
        "format":"fix-litesvm-apply-v2","source":"Unmodified LiteSVM 0.6.1 transactions, logs and self-CPI event instructions from the compiled fix SBF program",
        "programId":ID.to_string(),"slot":clock.slot,"unixTimestamp":clock.unix_timestamp,"lighthouseCaptured":h.lighthouse.is_some(),
        "transactions":transactions,"accounts":account_rows,
        "expected":{"mint":h.mint.pubkey().to_string(),"creator":h.creator.pubkey().to_string(),"trader":trader.pubkey().to_string(),
            "market":h.market.to_string(),"markets":[h.market.to_string(),extra_market.to_string()],"marketPhase":"curve","receiptCount":6,
            "receiptKinds":["created","created","buy","buy","sell","buy"],"protocolFees":protocol_fees.to_string(),
            "extension":ext.program.to_string(),"extensionRecipient":ext.recipient.to_string()}
    })).unwrap()).unwrap();
}

#[test]
fn pathological_curve_trades_and_receipt_impact_saturates() {
    // Extreme fixture values exercise telemetry bounds without overflowing funds.
    let mut fixture=terms();fixture.total_supply=1_000_000_000_000;fixture.virtual_token=fixture.total_supply;fixture.curve_tokens=fixture.total_supply-1;fixture.virtual_sol=1;fixture.curve_fee_bps=0;fixture.pool_fee_bps=0;assert!(fixture.validate().is_ok());
    let mut h=Harness::with_terms(fixture);let trader=h.wallet();h.svm.airdrop(&trader.pubkey(),1_100_000_000_000).unwrap();let token=h.token_account(trader.pubkey());let gross=fixture.curve_tokens;
    let traded=h.trade("buyExactIn",gross,1,&trader,token).unwrap();let r=receipt(&traded,1);
    assert_eq!(r.price_impact_bps,i64::MAX);assert_eq!(h.m().phase,1);h.assert_sol_accounting();assert_eq!(h.token_balance(token),fixture.curve_tokens);
    let file=std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/chain-vectors.json");
    let mut vectors:Value=std::fs::read_to_string(&file).ok().and_then(|text|serde_json::from_str(&text).ok()).unwrap_or_else(||json!({"source":"LiteSVM executed SBF receipts","vectors":[]}));
    let mut vector=h.vectors.pop().unwrap();vector["feeTerms"]=json!({"curveFeeBps":0,"poolFeeBps":0,"protocolShareBps":5000,"creatorShareBps":3000,"poolShareBps":2000});
    vectors["vectors"].as_array_mut().unwrap().push(vector);std::fs::write(file,serde_json::to_string_pretty(&vectors).unwrap()).unwrap();
    assert_eq!(math::impact(u128::MAX,1).unwrap(),-9_999);assert_eq!(math::impact(1,u128::MAX).unwrap(),i64::MAX);
}

#[test]
fn creation_initializes_curve_with_fixed_supply_and_no_purchase() {
    let mut h=Harness::new();h.assert_sol_accounting();assert_eq!(h.m().phase,0);assert_eq!(h.m().real_sol,0);assert_eq!(h.m().creator_fees,0);
    assert_eq!(h.token_balance(h.vault),terms().total_supply);assert_eq!(decode::<Protocol>(&h.svm,h.protocol).accrued,0);
    assert_eq!(h.svm.get_account(&h.market).unwrap().data.len(),214);assert_eq!(h.svm.get_account(&h.config).unwrap().data.len(),125);
    assert!(h.svm.get_account(&pda(&[b"prices",h.market.as_ref()])).is_none(),"creation allocates no price record");
    let mint_account=h.svm.get_account(&h.mint.pubkey()).unwrap();let mint=StateWithExtensions::<Mint>::unpack(&mint_account.data).unwrap();
    assert_eq!(mint.base.mint_authority,COption::None);assert_eq!(mint.base.freeze_authority,COption::None);assert_eq!(mint.base.supply,terms().total_supply);assert!(mint.get_extension_types().unwrap().len()>=2);
    let creator=h.creator.insecure_clone();let token=h.token_account(creator.pubkey());let traded=h.trade("buyExactIn",1_000_000,1,&creator,token).unwrap();assert!(receipt(&traded,1).tokens>0);
}

#[test]
fn create_and_buy_in_one_transaction_fails_atomically_for_every_buyer_and_mode() {
    for creator_buys in [true,false] {for exact_out in [false,true] {
        let mut h=Harness::initialized(terms());let creator=h.creator.insecure_clone();let mint=h.mint.insecure_clone();
        let other=h.wallet();let buyer=if creator_buys {&creator}else{&other};let token=Keypair::new();let rent:solana_sdk::rent::Rent=h.svm.get_sysvar();
        let receiver=solana_sdk::system_instruction::create_account(&h.payer.pubkey(),&token.pubkey(),rent.minimum_balance(TokenAccount::LEN),TokenAccount::LEN as u64,&spl_token_2022::ID);
        let initialize=spl_token_2022::instruction::initialize_account3(&spl_token_2022::ID,&token.pubkey(),&h.mint.pubkey(),&buyer.pubkey()).unwrap();
        let buy=if exact_out {h.trade_ix(instruction::BuyExactOut {tokens:1_000_000,max_gross:2_000_000},buyer.pubkey(),token.pubkey())}
            else{h.trade_ix(instruction::BuyExactIn {gross:1_000_000,min_tokens:1},buyer.pubkey(),token.pubkey())};
        let mut signers=vec![&creator,&mint,&token];if !creator_buys {signers.push(&other);}
        let before_creator=h.balance(creator.pubkey());let before_other=h.balance(other.pubkey());let protocol=h.svm.get_account(&h.protocol).unwrap();
        let refused=h.send(vec![h.create_ix(),receiver,initialize,buy],&signers).unwrap_err();assert!(refused.contains("BuyNotOpen"),"{refused}");
        for address in [h.market,h.mint.pubkey(),h.vault,token.pubkey()] {assert!(h.svm.get_account(&address).is_none());}
        assert_eq!(h.balance(creator.pubkey()),before_creator);assert_eq!(h.balance(other.pubkey()),before_other);assert_eq!(h.svm.get_account(&h.protocol).unwrap(),protocol);
    }}
}

#[test]
fn same_slot_buys_fail_for_every_buyer_and_mode_then_next_slot_matches_math() {
    let mut h=Harness::new();let created_slot=h.m().created_slot;let clock:Clock=h.svm.get_sysvar();assert_eq!(clock.slot,created_slot);
    let creator=h.creator.insecure_clone();let other=h.wallet();let ct=Keypair::new();let ot=Keypair::new();let rent:solana_sdk::rent::Rent=h.svm.get_sysvar();
    for (owner,token) in [(creator.pubkey(),&ct),(other.pubkey(),&ot)] {
        let allocate=solana_sdk::system_instruction::create_account(&h.payer.pubkey(),&token.pubkey(),rent.minimum_balance(TokenAccount::LEN),TokenAccount::LEN as u64,&spl_token_2022::ID);
        let initialize=spl_token_2022::instruction::initialize_account3(&spl_token_2022::ID,&token.pubkey(),&h.mint.pubkey(),&owner).unwrap();
        h.send_same_slot(vec![allocate,initialize],&[token]).unwrap();
    }
    let protected=[h.market,h.protocol,h.vault,h.mint.pubkey(),ct.pubkey(),ot.pubkey(),creator.pubkey(),other.pubkey()];
    let before:Vec<_>=protected.iter().map(|key|h.svm.get_account(key)).collect();
    for (buyer,token) in [(&creator,ct.pubkey()),(&other,ot.pubkey())] {for exact_out in [false,true] {
        let buy=if exact_out {h.trade_ix(instruction::BuyExactOut {tokens:1_000_000,max_gross:2_000_000},buyer.pubkey(),token)}
            else{h.trade_ix(instruction::BuyExactIn {gross:1_000_000,min_tokens:1},buyer.pubkey(),token)};
        let refused=h.send_same_slot(vec![buy],&[buyer]).unwrap_err();assert!(refused.contains("BuyNotOpen"),"{refused}");
        let after:Vec<_>=protected.iter().map(|key|h.svm.get_account(key)).collect();assert_eq!(before,after);
    }}
    h.advance_slot();let clock:Clock=h.svm.get_sysvar();assert_eq!(clock.slot,created_slot+1);
    for (buyer,token) in [(&creator,ct.pubkey()),(&other,ot.pubkey())] {for exact_out in [false,true] {
        let before=h.m();
        let (gross,output)=if exact_out {(math::gross_for_net(math::curve_buy_net(before.virtual_sol,before.virtual_token,1_000_000).unwrap(),terms().curve_fee_bps).unwrap(),1_000_000)}
            else{let fees=math::fees(1_000_000,terms().curve_fee_bps,&terms()).unwrap();(1_000_000,math::curve_buy_out(before.virtual_sol,before.virtual_token,1_000_000-fees.total).unwrap())};
        let fees=math::fees(gross,terms().curve_fee_bps,&terms()).unwrap();let wallet_before=h.balance(buyer.pubkey());let protocol_before=h.balance(h.protocol);
        let buy=if exact_out {h.trade_ix(instruction::BuyExactOut {tokens:output,max_gross:gross},buyer.pubkey(),token)}
            else{h.trade_ix(instruction::BuyExactIn {gross,min_tokens:output},buyer.pubkey(),token)};
        let executed=h.send_same_slot(vec![buy],&[buyer]).unwrap();let r=receipt(&executed,1);let after=h.m();
        assert_eq!((r.gross,r.tokens,r.protocol_fee,r.creator_fee,r.pool_fee),(gross,output,fees.protocol,fees.creator,fees.pool));
        assert_eq!(wallet_before-h.balance(buyer.pubkey()),gross);assert_eq!(h.balance(h.protocol)-protocol_before,fees.protocol);
        assert_eq!(after.real_sol-before.real_sol,r.net+fees.pool);assert_eq!(after.creator_fees-before.creator_fees,fees.creator);assert_eq!(after.created_slot,created_slot);h.assert_sol_accounting();
    }}
    let clock:Clock=h.svm.get_sysvar();assert_eq!(clock.slot,created_slot+1);
}

#[test]
fn prefunded_mint_and_vault_do_not_block_coin_creation() {
    let h=Harness::with_prefund(true);h.assert_sol_accounting();
    assert_eq!(h.token_balance(h.vault),terms().total_supply);
    let mint_account=h.svm.get_account(&h.mint.pubkey()).unwrap();
    let mint=StateWithExtensions::<Mint>::unpack(&mint_account.data).unwrap();
    assert_eq!(mint.base.mint_authority,COption::None);assert_eq!(mint.base.freeze_authority,COption::None);
}

#[test]
fn money_flows_quotes_fees_graduation_and_pool_all_four_modes() {
    let mut h=Harness::new();let trader=h.wallet();let token=h.token_account(trader.pubkey());
    let protocol_before=h.balance(h.protocol);let wallet_before=h.balance(trader.pubkey());let m0=h.m();
    let meta=h.trade("buyExactIn",10_000_003,1,&trader,token).unwrap();let r=receipt(&meta,1);let m1=h.m();
    assert_eq!(r.gross,r.net+r.protocol_fee+r.creator_fee+r.pool_fee);assert_eq!(wallet_before-h.balance(trader.pubkey()),r.gross);assert_eq!(h.balance(h.protocol)-protocol_before,r.protocol_fee);assert_eq!(m1.creator_fees-m0.creator_fees,r.creator_fee);assert_eq!(m1.real_sol-m0.real_sol,r.net+r.pool_fee);
    let split=math::fees(r.gross,terms().curve_fee_bps,&terms()).unwrap();assert_eq!(split.total,r.protocol_fee+r.creator_fee+r.pool_fee);assert_eq!(split.protocol,r.protocol_fee);
    h.trade("buyExactOut",1_000_003,u64::MAX,&trader,token).unwrap();h.trade("sellExactIn",100_003,0,&trader,token).unwrap();h.trade("sellExactOut",10_003,u64::MAX,&trader,token).unwrap();
    let before=h.m();let output=before.curve_remaining;let meta=h.trade("buyExactOut",output,u64::MAX,&trader,token).unwrap();let after=h.m();assert_eq!(after.phase,1);assert!(after.burned_tokens>0);let mint_data=h.svm.get_account(&h.mint.pubkey()).unwrap();let mint=StateWithExtensions::<Mint>::unpack(&mint_data.data).unwrap();assert_eq!(mint.base.supply,terms().total_supply-after.burned_tokens);assert_eq!(h.token_balance(h.vault),after.pool_tokens);assert_eq!(math::spot(after.real_sol,after.pool_token_num,after.pool_token_den).unwrap(),math::spot(after.virtual_sol,after.virtual_token as u128,1).unwrap());assert_eq!(receipts(&meta).iter().map(|r|r.action).collect::<Vec<_>>(),vec![4,1],"graduation receipt precedes its buy");
    h.trade("buyExactIn",1_000_003,1,&trader,token).unwrap();h.trade("buyExactOut",100_003,u64::MAX,&trader,token).unwrap();h.trade("sellExactIn",100_003,0,&trader,token).unwrap();h.trade("sellExactOut",10_003,u64::MAX,&trader,token).unwrap();
    let creator_signer=h.creator.insecure_clone();let before=h.balance(creator_signer.pubkey());let fees=h.m().creator_fees;let claim=ix(instruction::ClaimCreatorFees {},accounts::ClaimCreatorFees {creator:creator_signer.pubkey(),market:h.market,event_authority:event_authority(),program:ID});h.send(vec![claim.clone()],&[&creator_signer]).unwrap();assert_eq!(h.balance(creator_signer.pubkey())-before,fees);h.send(vec![claim],&[&creator_signer]).unwrap();assert_eq!(h.balance(creator_signer.pubkey())-before,fees);h.assert_sol_accounting();
    let mut new_terms=terms();new_terms.pool_fee_bps=9999;let update=ix(instruction::UpdateConfig {terms:new_terms,paused:true},accounts::AdminConfig {admin:creator_signer.pubkey(),config:h.config});h.send(vec![update],&[&creator_signer]).unwrap();assert_eq!(h.m().terms.pool_fee_bps,terms().pool_fee_bps);h.trade("buyExactIn",1_000_003,1,&trader,token).unwrap();h.write_vectors();
}

#[test]
fn math_splits_exactly_and_inverse_fee_handles_u64_edge() {
    for gross in [0,1,9,999,10_001,1_000_000_003,u64::MAX] { let a=math::fees(gross,333,&terms()).unwrap();assert_eq!(a.total,a.protocol+a.creator+a.pool);assert_eq!(a.protocol,((a.total as u128)*terms().protocol_share_bps as u128/10_000) as u64); }
    for rate in [0,1,333,9_999] {let net=u64::MAX-((u64::MAX as u128*rate as u128/10_000) as u64);let gross=math::gross_for_net(net,rate).unwrap();assert_eq!(gross-math::mul_div(gross,rate as u64,10_000).unwrap(),net);}
}


fn mainnet_terms() -> Terms {
    let text=std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/mainnet-terms.json")).unwrap();
    let value:Value=serde_json::from_str(&text).unwrap();let t=&value["terms"];
    let u=|key:&str|->u64 {t[key].as_str().map(|v|v.parse().unwrap()).unwrap_or_else(||t[key].as_u64().unwrap())};
    Terms { total_supply:u("total_supply"), decimals:u("decimals") as u8, virtual_sol:u("virtual_sol"), virtual_token:u("virtual_token"), curve_tokens:u("curve_tokens"),
        curve_fee_bps:u("curve_fee_bps") as u16, pool_fee_bps:u("pool_fee_bps") as u16, protocol_share_bps:u("protocol_share_bps") as u16, creator_share_bps:u("creator_share_bps") as u16, pool_share_bps:u("pool_share_bps") as u16 }
}

#[test]
fn heavy_transactions_keep_every_event_when_logs_truncate() {
    let mut h=Harness::with_terms(mainnet_terms());let trader=h.wallet();h.svm.airdrop(&trader.pubkey(),100_000_000_000).unwrap();let token=h.token_account(trader.pubkey());
    let mut sequence=h.m().receipt_sequence;
    for n in [4usize,8,12,16,20] {
        // The trader pays alone and relies on the default budget so twenty buys fit one packet.
        h.advance_slot();h.svm.expire_blockhash();
        let buys:Vec<Instruction>=(0..n).map(|i|h.trade_ix(instruction::BuyExactIn {gross:10_000_000+i as u64,min_tokens:1},trader.pubkey(),token)).collect();
        let tx=Transaction::new_signed_with_payer(&buys,Some(&trader.pubkey()),&[&trader],h.svm.latest_blockhash());
        let before=h.svm.get_account(&h.market).unwrap();
        match h.svm.send_transaction(tx) {
            Ok(meta) => {
                let truncated=meta.logs.iter().any(|line|line=="Log truncated");let kept=receipts(&meta);
                println!("{n} buys in one transaction: CU={} logs truncated={truncated} receipts={}",meta.compute_units_consumed,kept.len());
                assert_eq!(kept.len(),n,"every buy keeps its receipt");
                for r in &kept {sequence+=1;assert_eq!((r.action,r.sequence,r.actor),(1,sequence,trader.pubkey()));}
                if n==16 {assert!(truncated,"sixteen buys exceed the 10,000-byte log budget");}
            }
            // Each buy takes four trace entries (itself, two transfers, its event); past 64 the
            // runtime refuses the whole transaction, so no state moves and nothing goes unrecorded.
            Err(failure) => {
                println!("{n} buys in one transaction: refused with {:?}",failure.err);
                assert!(n>16,"{n} buys fit the instruction trace");
                assert_eq!(failure.err,solana_sdk::transaction::TransactionError::InstructionError(16,solana_sdk::instruction::InstructionError::MaxInstructionTraceLengthExceeded));
                assert_eq!(h.svm.get_account(&h.market).unwrap(),before);
            }
        }
        h.assert_sol_accounting();
    }
    // Foreign logs ahead of a creation fill the log budget; CoinCreated rides in instruction data.
    let mint=fixture_mint(1);let market=pda(&[b"market",mint.pubkey().as_ref()]);let vault=pda(&[b"vault",market.as_ref()]);
    let memo:Pubkey="MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr".parse().unwrap();
    let mut spam:Vec<Instruction>=(0..40).map(|_|Instruction {program_id:memo,accounts:vec![AccountMeta::new_readonly(h.payer.pubkey(),true)],data:vec![b'x']}).collect();
    spam.push(ix(instruction::CreateCoin {name:"Hidden".into(),symbol:"HID".into(),uri:"https://example.invalid/h.json".into(),expected_config_revision:0},accounts::CreateCoin {creator:h.creator.pubkey(),config:h.config,mint:mint.pubkey(),market,vault,token_program:spl_token_2022::ID,system_program:solana_sdk::system_program::ID,event_authority:event_authority(),program:ID}));
    let creator=h.creator.insecure_clone();let meta=h.send(spam,&[&creator,&mint]).unwrap();
    let created=events(&meta).iter().filter(|b|b.starts_with(CoinCreated::DISCRIMINATOR)).map(|b|CoinCreated::deserialize(&mut &b[8..]).unwrap()).collect::<Vec<_>>();
    let truncated=meta.logs.iter().any(|line|line=="Log truncated");
    println!("create after 40 memos: logs truncated={truncated} CoinCreated={}",created.len());
    assert!(truncated);assert_eq!(created.len(),1);assert_eq!(created[0].mint,mint.pubkey());assert_eq!(receipt(&meta,0).market,market);
}

#[test]
fn compute_units_per_action_are_measured() {
    // Mainnet terms and maximum metadata lengths: the service sizes each action's limit from these.
    let mut h=Harness::initialized(mainnet_terms());let creator=h.creator.insecure_clone();let mint=h.mint.insecure_clone();
    let create=ix(instruction::CreateCoin {name:"N".repeat(64),symbol:"S".repeat(16),uri:format!("https://example.invalid/{}",String::from("u").repeat(232)),expected_config_revision:0},accounts::CreateCoin {creator:h.creator.pubkey(),config:h.config,mint:h.mint.pubkey(),market:h.market,vault:h.vault,token_program:spl_token_2022::ID,system_program:solana_sdk::system_program::ID,event_authority:event_authority(),program:ID});
    let created=h.send(vec![create],&[&creator,&mint]).unwrap();
    let shard=(protocol_shard(&h.market)+1)%PROTOCOL_SHARDS;let opened=h.send(vec![h.shard_ix(shard)],&[]).unwrap();
    let trader=h.wallet();h.svm.airdrop(&trader.pubkey(),200_000_000_000).unwrap();let token=h.token_account(trader.pubkey());
    let cu=|meta:TransactionMetadata|meta.compute_units_consumed;
    let curve_buy=cu(h.trade("buyExactIn",1_000_000_000,1,&trader,token).unwrap()).max(cu(h.trade("buyExactOut",1_000_000_000_000,u64::MAX,&trader,token).unwrap()));
    let curve_sell=cu(h.trade("sellExactIn",100_000_000_000,1,&trader,token).unwrap()).max(cu(h.trade("sellExactOut",10_000_000,u64::MAX,&trader,token).unwrap()));
    let graduating=h.trade("buyExactIn",150_000_000_000,1,&trader,token).unwrap();assert_eq!(h.m().phase,1);let graduating_buy=cu(graduating);
    let pool_buy=cu(h.trade("buyExactIn",1_000_000_000,1,&trader,token).unwrap()).max(cu(h.trade("buyExactOut",1_000_000_000_000,u64::MAX,&trader,token).unwrap()));
    let pool_sell=cu(h.trade("sellExactIn",100_000_000_000,1,&trader,token).unwrap()).max(cu(h.trade("sellExactOut",10_000_000,u64::MAX,&trader,token).unwrap()));
    let claim=ix(instruction::ClaimCreatorFees {},accounts::ClaimCreatorFees {creator:creator.pubkey(),market:h.market,event_authority:event_authority(),program:ID});
    let claimed=cu(h.send(vec![claim],&[&creator]).unwrap());
    let report=json!({"source":"LiteSVM 0.6.1, compiled fix SBF, mainnet launch terms; each figure includes the 1.4M SetComputeUnitLimit instruction",
        "createCoinMaxMetadata":cu(created),"createProtocolShard":cu(opened),"curveBuy":curve_buy,"graduatingBuy":graduating_buy,"poolBuy":pool_buy,"curveSell":curve_sell,"poolSell":pool_sell,"claimCreatorFees":claimed});
    std::fs::write(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/compute-units.json"),serde_json::to_string_pretty(&report).unwrap()).unwrap();
    println!("Compute units: {report}");
    // sdk/src/compute.ts carries these ceilings; a measured action above one fails here first.
    for (name,ceiling) in [("createCoinMaxMetadata",80_000u64),("curveBuy",60_000),("graduatingBuy",80_000),("poolBuy",60_000),("curveSell",60_000),("poolSell",60_000),("claimCreatorFees",20_000)] {
        assert!(report[name].as_u64().unwrap()<=ceiling,"{name} exceeds its SDK ceiling {ceiling}: {report}");
    }
}

#[test]
fn extensions_route_trades_and_take_their_share_only_from_the_protocol_part() {
    // Two identical markets: one trades plain, one through the extension, with the same inputs.
    let mut plain=Harness::new();let mut routed=Harness::new();let ext=routed.extension(2_500);
    let (pt,rt)=(plain.wallet(),routed.wallet());let (ptoken,rtoken)=(plain.token_account(pt.pubkey()),routed.token_account(rt.pubkey()));
    for (mode,amount,bound) in [("buyExactIn",10_000_003u64,1u64),("buyExactOut",1_000_003,u64::MAX),("sellExactIn",100_003,0),("sellExactOut",10_003,u64::MAX)] {
        let plain_meta=plain.trade(mode,amount,bound,&pt,ptoken).unwrap();
        let core=match mode { "buyExactIn"=>routed.trade_ix(instruction::BuyExactIn {gross:amount,min_tokens:bound},rt.pubkey(),rtoken), "buyExactOut"=>routed.trade_ix(instruction::BuyExactOut {tokens:amount,max_gross:bound},rt.pubkey(),rtoken), "sellExactIn"=>routed.trade_ix(instruction::SellExactIn {tokens:amount,min_sol:bound},rt.pubkey(),rtoken), _=>routed.trade_ix(instruction::SellExactOut {sol:amount,max_tokens:bound},rt.pubkey(),rtoken) };
        // A plain trade keeps its account list: the IDL's ten, nothing appended.
        assert_eq!(core.accounts.len(),10);
        let (wallet_before,shard_before,recipient_before)=(routed.balance(rt.pubkey()),routed.balance(routed.protocol),routed.balance(ext.recipient));
        routed.advance_slot();let tx=routed.routed(&ext,0,core,ext.recipient);let routed_meta=routed.send_same_slot(vec![tx],&[&rt]).unwrap();
        let action=if mode.starts_with("buy") {1} else {2};let (p,r)=(receipt(&plain_meta,action),receipt(&routed_meta,action));
        // The trader, the creator and the pool see the same amounts; the protocol part is divided.
        assert_eq!((r.gross,r.net,r.tokens,r.refund,r.creator_fee,r.pool_fee,r.price_after),(p.gross,p.net,p.tokens,p.refund,p.creator_fee,p.pool_fee,p.price_after),"{mode}");
        assert_eq!(r.protocol_fee+r.extension_fee,p.protocol_fee);assert_eq!(r.extension_fee,p.protocol_fee*2_500/10_000);
        assert_eq!(r.gross,r.net+r.protocol_fee+r.extension_fee+r.creator_fee+r.pool_fee,"every line is itemized");
        assert_eq!((r.extension,p.extension,p.extension_fee),(ext.program,Pubkey::default(),0));
        assert_eq!(routed.balance(ext.recipient)-recipient_before,r.extension_fee);assert_eq!(routed.balance(routed.protocol)-shard_before,r.protocol_fee);
        let moved=if action==1 {wallet_before-routed.balance(rt.pubkey())} else {routed.balance(rt.pubkey())-wallet_before};
        assert_eq!(moved,if action==1 {p.gross} else {p.net});
        routed.assert_sol_accounting();
    }
    assert_eq!(decode::<Protocol>(&routed.svm,routed.protocol).accrued+routed.balance(ext.recipient)-1_000_000_000,decode::<Protocol>(&plain.svm,plain.protocol).accrued);
}

#[test]
fn extension_refusals_are_named_and_plain_trades_never_depend_on_the_registry() {
    let mut h=Harness::new();let admin=h.creator.insecure_clone();let trader=h.wallet();let token=h.token_account(trader.pubkey());
    let refused=|h:&mut Harness,tx:Instruction,signers:&[&Keypair]|h.send(vec![tx],signers).unwrap_err();
    let buy=|h:&Harness|h.trade_ix(instruction::BuyExactIn {gross:1_000_000,min_tokens:1},trader.pubkey(),token);
    // Not registered: the entry account does not exist.
    let program=Pubkey::new_from_array([0x7e;32]);let path=std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/deploy/fix_test_extension.so");h.svm.add_program_from_file(program,&path).unwrap();
    let unregistered=Extension {program,entry:pda(&[b"extension",program.as_ref()]),authority:Pubkey::find_program_address(&[b"fix-extension"],&program).0,recipient:Keypair::new().pubkey()};
    let tx=h.routed(&unregistered,0,buy(&h),unregistered.recipient);assert!(refused(&mut h,tx,&[&trader]).contains("ExtensionNotRegistered"));
    // Registration is the admin's, and a share is at most the whole protocol part.
    let outsider=h.wallet();let config=h.config;assert!(refused(&mut h,unregistered.register_ix(outsider.pubkey(),config,100),&[&outsider]).contains("Unauthorized"));
    assert!(refused(&mut h,unregistered.register_ix(admin.pubkey(),config,10_001),&[&admin]).contains("InvalidExtensionShare"));
    let ext=h.extension(1_000);
    // Wrong number of routing accounts.
    let mut two=buy(&h);two.accounts.extend([AccountMeta::new_readonly(ext.entry,false),AccountMeta::new(ext.recipient,false)]);
    assert!(refused(&mut h,two,&[&trader]).contains("InvalidExtensionAccounts"));
    // An authority that is not the extension's PDA, signing at top level.
    let impostor=Keypair::new();let mut forged=buy(&h);forged.accounts.extend([AccountMeta::new_readonly(ext.entry,false),AccountMeta::new_readonly(impostor.pubkey(),true),AccountMeta::new(ext.recipient,false)]);
    assert!(refused(&mut h,forged,&[&trader,&impostor]).contains("ExtensionAuthorityInvalid"));
    // The right PDA, passed by its program without signing.
    let tx=h.routed(&ext,1,buy(&h),ext.recipient);assert!(refused(&mut h,tx,&[&trader]).contains("ExtensionAuthorityInvalid"));
    // A share account other than the registered recipient.
    let elsewhere=Keypair::new().pubkey();h.svm.airdrop(&elsewhere,1_000_000_000).unwrap();
    let tx=h.routed(&ext,0,buy(&h),elsewhere);assert!(refused(&mut h,tx,&[&trader]).contains("ExtensionRecipientMismatch"));
    // Pausing stops routed trades only.
    h.send(vec![ext.pause_ix(admin.pubkey(),h.config,true)],&[&admin]).unwrap();
    let tx=h.routed(&ext,0,buy(&h),ext.recipient);assert!(refused(&mut h,tx,&[&trader]).contains("ExtensionPaused"));
    let plain=h.trade("buyExactIn",1_000_000,1,&trader,token).unwrap();assert_eq!(receipt(&plain,1).extension_fee,0);
    h.send(vec![ext.pause_ix(admin.pubkey(),h.config,false)],&[&admin]).unwrap();
    let tx=h.routed(&ext,0,buy(&h),ext.recipient);let resumed=h.send(vec![tx],&[&trader]).unwrap();assert_eq!(receipt(&resumed,1).extension,ext.program);
    // Removal closes the entry and returns its rent; routed trades stop, plain trades go on, nothing else moves.
    let market=h.svm.get_account(&h.market).unwrap();let admin_before=h.balance(admin.pubkey());let entry_rent=h.balance(ext.entry);
    h.send(vec![ext.remove_ix(admin.pubkey(),h.config)],&[&admin]).unwrap();
    assert!(h.svm.get_account(&ext.entry).is_none_or(|a|a.lamports==0&&a.owner==solana_sdk::system_program::ID),"the entry is closed");assert_eq!(h.balance(admin.pubkey())-admin_before,entry_rent);assert_eq!(h.svm.get_account(&h.market).unwrap(),market);
    let tx=h.routed(&ext,0,buy(&h),ext.recipient);assert!(refused(&mut h,tx,&[&trader]).contains("ExtensionNotRegistered"));
    h.trade("sellExactIn",100_000,0,&trader,token).unwrap();h.assert_sol_accounting();
    use fix_launchpad::LaunchError as E;
    assert_eq!([E::InvalidExtensionAccounts,E::ExtensionNotRegistered,E::ExtensionPaused,E::ExtensionAuthorityInvalid,E::ExtensionRecipientMismatch,E::InvalidExtensionShare].map(u32::from),[6012,6013,6014,6015,6016,6017],"appended codes keep earlier numbers");
}
