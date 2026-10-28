use anchor_lang::{AccountDeserialize, AccountSerialize, AnchorDeserialize, Discriminator, InstructionData, ToAccountMetas};
use base64::{engine::general_purpose::STANDARD, Engine};
use fix_launchpad::{accounts, instruction, math, Call, CallerSequence, CallPosition, CallSettled, CoinCreated, Market, PriceRecorded, Protocol, Receipt, Terms, ID};
use litesvm::{types::TransactionMetadata, LiteSVM};
use serde_json::{json, Value};
use solana_sdk::{account::Account, bpf_loader_upgradeable::{self, UpgradeableLoaderState}, clock::Clock, compute_budget::ComputeBudgetInstruction, instruction::Instruction, pubkey::Pubkey, signature::{Keypair, Signer}, transaction::Transaction};
use anchor_lang::solana_program::{program_option::COption, program_pack::Pack};
use spl_token_2022::{extension::{BaseStateWithExtensions, StateWithExtensions}, state::{Account as TokenAccount, Mint}};

// Fixture values only. These are deliberately small and are not a launch recommendation.
fn terms() -> Terms { Terms { total_supply: 1_000_000_000, decimals: 6,
    virtual_sol: 100_000_000, virtual_token: 1_000_000_000, curve_tokens: 600_000_000,
    curve_fee_bps: 300, pool_fee_bps: 100,
    protocol_share_bps: 5_000, creator_share_bps: 3_000, pool_share_bps: 2_000, call_share_bps: 5_000,
    min_horizon: 60, max_horizon: 3_600, scoring_window: 60 } }
fn pda(seeds: &[&[u8]]) -> Pubkey { Pubkey::find_program_address(seeds, &ID).0 }
fn ix(data: impl InstructionData, accs: impl ToAccountMetas) -> Instruction { Instruction { program_id: ID, data: data.data(), accounts: accs.to_account_metas(None) } }
fn decode<T: AccountDeserialize>(svm: &LiteSVM, key: Pubkey) -> T { let a = svm.get_account(&key).unwrap(); T::try_deserialize(&mut a.data.as_slice()).unwrap() }
fn receipt(meta: &TransactionMetadata, action: u8) -> Receipt {
    meta.logs.iter().filter_map(|l| l.strip_prefix("Program data: ")).filter_map(|s| STANDARD.decode(s).ok())
        .filter(|b| b.starts_with(Receipt::DISCRIMINATOR))
        .filter_map(|b| Receipt::deserialize(&mut &b[8..]).ok()).find(|r| r.action == action)
        .unwrap_or_else(|| panic!("Missing receipt action {action}: {:?}", meta.logs))
}
fn call_settled(meta:&TransactionMetadata) -> CallSettled {
    meta.logs.iter().filter_map(|line|line.strip_prefix("Program data: ")).filter_map(|data|STANDARD.decode(data).ok())
        .filter(|bytes|bytes.starts_with(CallSettled::DISCRIMINATOR)).filter_map(|bytes|CallSettled::deserialize(&mut &bytes[8..]).ok()).next()
        .unwrap_or_else(||panic!("Missing call settlement event: {:?}",meta.logs))
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
        let mut svm = LiteSVM::new();
        let artifact = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/deploy/fix_launchpad.so");
        assert!(artifact.exists(), "Build SBF first: cargo build-sbf --manifest-path programs/fix-launchpad/Cargo.toml --arch v0");
        let elf = std::fs::read(artifact).unwrap();
        let payer = Keypair::new(); let creator = Keypair::new(); let mint = Keypair::new();
        svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap(); svm.airdrop(&creator.pubkey(), 10_000_000_000).unwrap();
        let program_data=Pubkey::find_program_address(&[ID.as_ref()],&bpf_loader_upgradeable::ID).0;
        let mut program_bytes=bincode::serialize(&UpgradeableLoaderState::ProgramData { slot:0, upgrade_authority_address:Some(creator.pubkey()) }).unwrap(); program_bytes.resize(UpgradeableLoaderState::size_of_programdata_metadata(),0); program_bytes.extend_from_slice(&elf);
        let rent:solana_sdk::rent::Rent=svm.get_sysvar(); svm.set_account(program_data,Account { lamports:rent.minimum_balance(program_bytes.len()),data:program_bytes,owner:bpf_loader_upgradeable::ID,executable:false,rent_epoch:0 }).unwrap();
        let data=bincode::serialize(&UpgradeableLoaderState::Program { programdata_address:program_data }).unwrap(); svm.set_account(ID,Account { lamports:rent.minimum_balance(data.len()),data,owner:bpf_loader_upgradeable::ID,executable:true,rent_epoch:0 }).unwrap();
        let config = pda(&[b"config"]); let protocol = pda(&[b"protocol"]); let market = pda(&[b"market", mint.pubkey().as_ref()]); let vault = pda(&[b"vault", market.as_ref()]);
        let mut h = Self { svm, payer, creator, config, protocol, mint, market, vault, vectors: vec![], recorded: vec![], lighthouse: None }; h.time(1_000);
        let init = ix(instruction::InitializeConfig { terms: fixture }, accounts::InitializeConfig { admin: h.creator.pubkey(), program:ID, program_data, config, protocol, system_program: solana_sdk::system_program::ID });
        let creator_signer=h.creator.insecure_clone();
        h.send(vec![init], &[&creator_signer]).unwrap();
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
    fn create_ix(&self) -> Instruction {
        ix(instruction::CreateCoin {name:"Fixture Coin".into(),symbol:"FIXT".into(),uri:"https://example.invalid/fixture.json".into(),expected_config_revision:0},accounts::CreateCoin {creator:self.creator.pubkey(),config:self.config,mint:self.mint.pubkey(),market:self.market,price_history:self.history_address(),vault:self.vault,token_program:spl_token_2022::ID,system_program:solana_sdk::system_program::ID})
    }
    fn advance_slot(&mut self) {let mut clock:Clock=self.svm.get_sysvar();clock.slot=clock.slot.checked_add(1).unwrap();self.svm.set_sysvar(&clock);}
    fn send(&mut self,ixs:Vec<Instruction>,signers:&[&Keypair])->Result<TransactionMetadata,String> {
        self.advance_slot();self.send_same_slot(ixs,signers)
    }
    fn send_same_slot(&mut self, mut ixs: Vec<Instruction>, signers: &[&Keypair]) -> Result<TransactionMetadata, String> {
        self.svm.expire_blockhash(); ixs.insert(0, ComputeBudgetInstruction::set_compute_unit_limit(1_400_000));
        if let Some(program_id) = self.lighthouse {
            // Actual Lighthouse AssertAccountInfo: plaintext lamports == fixed ring rent.
            let history = self.history_address();
            let mut data = vec![5, 1, 0]; data.extend_from_slice(&self.balance(history).to_le_bytes()); data.push(0);
            ixs.push(Instruction { program_id, accounts: vec![solana_sdk::instruction::AccountMeta::new_readonly(history, false)], data });
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
    fn history_address(&self) -> Pubkey { pda(&[b"prices",self.market.as_ref()]) }
    fn trade_ix(&self,data:impl InstructionData,who:Pubkey,token:Pubkey,tag:Option<(Pubkey,Pubkey)>) -> Instruction {
        ix(data,accounts::Trade { trader:who,market:self.market,mint:self.mint.pubkey(),vault:self.vault,trader_token:token,protocol:self.protocol,position:tag.map(|t|t.1),price_history:self.history_address(),token_program:spl_token_2022::ID,system_program:solana_sdk::system_program::ID })
    }
    fn trade(&mut self,mode:&str,amount:u64,bound:u64,who:&Keypair,token:Pubkey,tag:Option<(Pubkey,Pubkey)>) -> Result<TransactionMetadata,String> {
        self.advance_slot();
        let before=self.snapshot();
        let ins=match mode { "buyExactIn"=>self.trade_ix(instruction::BuyExactIn { gross:amount,min_tokens:bound },who.pubkey(),token,tag), "buyExactOut"=>self.trade_ix(instruction::BuyExactOut { tokens:amount,max_gross:bound },who.pubkey(),token,tag), "sellExactIn"=>self.trade_ix(instruction::SellExactIn { tokens:amount,min_sol:bound },who.pubkey(),token,None), "sellExactOut"=>self.trade_ix(instruction::SellExactOut { sol:amount,max_tokens:bound },who.pubkey(),token,None), _=>panic!("unknown mode") };
        let meta=self.send_same_slot(vec![ins],&[who])?;
        self.assert_sol_accounting();
        let action=if mode.starts_with("buy") { 1 } else { 2 };
        let r=receipt(&meta,action);
        self.vectors.push(json!({"mode":mode,"amount":amount.to_string(),"tagged":tag.is_some(),"before":before,"after":self.snapshot(),"receipt":receipt_json(&r)})); Ok(meta)
    }
    fn snapshot(&self) -> Value { let m=self.m();let clock:Clock=self.svm.get_sysvar(); json!({"createdSlot":m.created_slot.to_string(),"slot":clock.slot.to_string(),"phase":match m.phase {0=>"curve",1=>"pool",_=>panic!("invalid phase")},"virtualSol":m.virtual_sol.to_string(),"virtualToken":m.virtual_token.to_string(),"curveRemaining":m.curve_remaining.to_string(),"realSol":m.real_sol.to_string(),"poolTokenNumerator":m.pool_token_num.to_string(),"poolTokenDenominator":m.pool_token_den.to_string(),"physicalTokens":self.token_balance(self.vault).to_string()}) }
    fn open_call(&mut self,caller:&Keypair,id:u64,horizon:i64) -> Pubkey {
        let call=pda(&[b"call",self.market.as_ref(),caller.pubkey().as_ref(),&id.to_le_bytes()]);
        let ins=ix(instruction::OpenCall { id,horizon_seconds:horizon },accounts::OpenCall {caller:caller.pubkey(),market:self.market,caller_sequence:pda(&[b"call-sequence",self.market.as_ref(),caller.pubkey().as_ref()]),call,system_program:solana_sdk::system_program::ID});self.send(vec![ins],&[caller]).unwrap();call
    }
    fn mature(&mut self,call:Pubkey) { let state:Call=decode(&self.svm,call);self.time(state.horizon); }
    fn position(&mut self,call:Pubkey,who:&Keypair) -> Pubkey {
        let position=pda(&[b"position",call.as_ref(),who.pubkey().as_ref()]);
        let ins=ix(instruction::PreparePosition {},accounts::PreparePosition {payer:who.pubkey(),follower:who.pubkey(),market:self.market,call,position,system_program:solana_sdk::system_program::ID});self.send(vec![ins],&[who]).unwrap();position
    }
    fn checkpoint(&mut self,call_key:Pubkey) -> Result<TransactionMetadata,String> {
        let ins=ix(instruction::CheckpointCall {},accounts::CheckpointCall {settler:self.payer.pubkey(),market:self.market,call:call_key,price_history:self.history_address()});
        self.send(vec![ins],&[])
    }
    fn settle_call(&mut self,call:Pubkey,position:Pubkey,caller:Pubkey,follower:Pubkey) -> Result<TransactionMetadata,String> {
        let ins=ix(instruction::SettleCall {},accounts::SettleCall {settler:self.payer.pubkey(),market:self.market,call,position,caller,follower,price_history:self.history_address()});self.send(vec![ins],&[])
    }
    fn write_vectors(&self) { let file=std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/chain-vectors.json"); std::fs::write(file,serde_json::to_string_pretty(&json!({"source":"LiteSVM executed SBF receipts","feeTerms":{"curveFeeBps":300,"poolFeeBps":100,"protocolShareBps":5000,"creatorShareBps":3000,"poolShareBps":2000,"callShareBps":5000},"vectors":self.vectors})).unwrap()).unwrap(); }
    fn write_price_vector(&self,label:&str,call:Pubkey,position:Pubkey,integral:Option<math::U256>) {
        let file=std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/price-record-vectors.json");
        let mut vectors:Value=std::fs::read_to_string(&file).ok().and_then(|text|serde_json::from_str(&text).ok()).unwrap_or_else(||json!({}));
        let timing:Call=decode(&self.svm,call);let clock:Clock=self.svm.get_sysvar();
        vectors[label]=json!({"source":"LiteSVM executed SBF price record","marketBase64":STANDARD.encode(self.svm.get_account(&self.market).unwrap().data),"historyBase64":STANDARD.encode(self.svm.get_account(&self.history_address()).unwrap().data),"callBase64":STANDARD.encode(self.svm.get_account(&call).unwrap().data),"positionBase64":STANDARD.encode(self.svm.get_account(&position).unwrap().data),"windowStart":timing.window_start.to_string(),"horizon":timing.horizon.to_string(),"now":clock.unix_timestamp.to_string(),"expectedIntegral":integral.map(|value|value.to_string())});
        std::fs::write(file,serde_json::to_string_pretty(&vectors).unwrap()).unwrap();
    }
    fn write_price_outcome(&self,label:&str,outcome:&CallSettled) {
        let file=std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/price-record-vectors.json");
        let mut vectors:Value=serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        vectors[label]["expectedValue"]=json!(outcome.exit_value.to_string());vectors[label]["expectedProfitable"]=json!(outcome.profitable);vectors[label]["expectedRefund"]=json!((if outcome.profitable {0}else{outcome.payout}).to_string());
        std::fs::write(file,serde_json::to_string_pretty(&vectors).unwrap()).unwrap();
    }
}
fn receipt_json(r:&Receipt)->Value { json!({"gross":r.gross.to_string(),"protocol":r.protocol_fee.to_string(),"creator":r.creator_fee.to_string(),"call":r.call_fee.to_string(),"pool":r.pool_fee.to_string(),"net":r.net.to_string(),"tokens":r.tokens.to_string(),"refund":r.refund.to_string(),"beforePrice":r.price_before.to_string(),"afterPrice":r.price_after.to_string(),"priceImpactBps":r.price_impact_bps.to_string()}) }

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
    let extra_mint=Keypair::new();let extra_market=pda(&[b"market",extra_mint.pubkey().as_ref()]);
    let extra_vault=pda(&[b"vault",extra_market.as_ref()]);let extra_history=pda(&[b"prices",extra_market.as_ref()]);
    let create=ix(instruction::CreateCoin {name:"Empty Fixture Coin".into(),symbol:"EMPTY".into(),uri:"https://example.invalid/empty.json".into(),expected_config_revision:0},accounts::CreateCoin {creator:h.creator.pubkey(),config:h.config,mint:extra_mint.pubkey(),market:extra_market,price_history:extra_history,vault:extra_vault,token_program:spl_token_2022::ID,system_program:solana_sdk::system_program::ID});
    let creator_signer=h.creator.insecure_clone();h.send(vec![create],&[&creator_signer,&extra_mint]).unwrap();
    selected.push((h.recorded.len()-1,"create_coin"));
    let creator=h.creator.insecure_clone();let creator_token=h.token_account(creator.pubkey());
    h.trade("buyExactIn",2_000_000,1,&creator,creator_token,None).unwrap();selected.push((h.recorded.len()-1,"creator_buy"));
    let trader=h.wallet();let token=h.token_account(trader.pubkey());
    h.time(1_001);h.trade("buyExactIn",3_000_000,1,&trader,token,None).unwrap();selected.push((h.recorded.len()-1,"buy"));
    h.time(1_002);h.trade("sellExactIn",1_000_000,0,&trader,token,None).unwrap();selected.push((h.recorded.len()-1,"sell"));
    let mut protocol_fees=0u64;
    let transactions:Vec<Value>=selected.into_iter().map(|(index,name)| {
        let mut item=h.recorded[index].clone();item["name"]=json!(name);let logs=item["logs"].as_array().unwrap();
        assert!(logs.iter().any(|line|line.as_str().unwrap().starts_with("Program log: Instruction: ")));
        assert!(logs.iter().any(|line|line.as_str().unwrap().starts_with(&format!("Program {} consumed ",ID))));
        let mut names=vec![];let mut actions=vec![];let mut receipts=vec![];
        for line in logs {
            if let Some(data)=line.as_str().unwrap().strip_prefix("Program data: ") {
                let bytes=STANDARD.decode(data).unwrap();
                let event=if bytes.starts_with(CoinCreated::DISCRIMINATOR) {"CoinCreated"}
                    else if bytes.starts_with(PriceRecorded::DISCRIMINATOR) {"PriceRecorded"}
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
    let account_keys=[h.config,h.protocol,h.market,h.history_address(),h.mint.pubkey(),h.vault,token,creator_token,extra_market,extra_history,extra_mint.pubkey(),extra_vault];
    let account_rows:Vec<Value>=account_keys.iter().filter_map(|key|h.svm.get_account(key).map(|account|json!({
        "address":key.to_string(),"owner":account.owner.to_string(),"lamports":account.lamports.to_string(),"dataBase64":STANDARD.encode(&account.data),"executable":account.executable,
    }))).collect();
    let clock:Clock=h.svm.get_sysvar();let file=std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/event-log-vectors.json");
    std::fs::write(file,serde_json::to_string_pretty(&json!({
        "format":"fix-litesvm-apply-v1","source":"Unmodified LiteSVM 0.6.1 metadata logs from the compiled fix SBF program",
        "programId":ID.to_string(),"slot":clock.slot,"unixTimestamp":clock.unix_timestamp,"lighthouseCaptured":h.lighthouse.is_some(),
        "transactions":transactions,"accounts":account_rows,
        "expected":{"mint":h.mint.pubkey().to_string(),"creator":h.creator.pubkey().to_string(),"trader":trader.pubkey().to_string(),
            "market":h.market.to_string(),"markets":[h.market.to_string(),extra_market.to_string()],"marketPhase":"curve","receiptCount":5,
            "receiptKinds":["created","created","buy","buy","sell"],"protocolFees":protocol_fees.to_string()}
    })).unwrap()).unwrap();
}

#[test]
fn caller_sequences_cannot_be_raced_by_other_callers() {
    let mut h=Harness::new();let alice=h.wallet();let other=h.wallet();
    let call=pda(&[b"call",h.market.as_ref(),alice.pubkey().as_ref(),&0u64.to_le_bytes()]);
    let sequence=pda(&[b"call-sequence",h.market.as_ref(),alice.pubkey().as_ref()]);
    let prepared=ix(instruction::OpenCall {id:0,horizon_seconds:60},accounts::OpenCall {caller:alice.pubkey(),market:h.market,caller_sequence:sequence,call,system_program:solana_sdk::system_program::ID});
    for id in 0..8 {h.open_call(&other,id,60);}
    h.send(vec![prepared.clone()],&[&alice]).unwrap();
    assert_eq!(decode::<CallerSequence>(&h.svm,sequence).next_id,1);
    assert_eq!(decode::<CallerSequence>(&h.svm,pda(&[b"call-sequence",h.market.as_ref(),other.pubkey().as_ref()])).next_id,8);
    assert!(h.send(vec![prepared],&[&alice]).is_err());
    h.open_call(&other,8,60);h.open_call(&alice,1,60);
    assert_eq!(decode::<CallerSequence>(&h.svm,sequence).next_id,2);
}

#[test]
fn maximum_scoring_window_keeps_two_hours_of_checkpoint_margin() {
    let mut fixture=terms();fixture.scoring_window=7_200;fixture.min_horizon=7_200;fixture.max_horizon=14_400;
    assert!(fixture.validate().is_ok());let mut rejected=fixture;rejected.scoring_window=7_260;assert!(rejected.validate().is_err());
    let mut h=Harness::with_terms(fixture);let caller=h.wallet();let follower=h.wallet();let whale=h.wallet();let ft=h.token_account(follower.pubkey());let wt=h.token_account(whale.pubkey());
    let call=h.open_call(&caller,0,7_200);let position=h.position(call,&follower);
    h.trade("buyExactIn",1_000_000,1,&follower,ft,Some((call,position))).unwrap();h.trade("buyExactIn",80_000_000,1,&whale,wt,None).unwrap();
    let timing:Call=decode(&h.svm,call);h.time(timing.horizon+7_200);
    h.trade("buyExactIn",50_000,1,&whale,wt,None).unwrap();h.checkpoint(call).unwrap();
    let checkpoint:Call=decode(&h.svm,call);assert!(checkpoint.checkpointed && !checkpoint.evidence_missing);
    let settled=h.settle_call(call,position,caller.pubkey(),follower.pubkey()).unwrap();assert!(call_settled(&settled).profitable);
}

#[test]
fn exact_break_even_refunds_the_follower() {
    let mut h=Harness::new();let caller=h.wallet();let follower=h.wallet();let token=h.token_account(follower.pubkey());let call=h.open_call(&caller,0,60);let position=h.position(call,&follower);
    h.trade("buyExactIn",1_000_000,1,&follower,token,Some((call,position))).unwrap();let held=h.balance(position);let pos:CallPosition=decode(&h.svm,position);h.mature(call);
    // A precise boundary fixture makes the integer exit mark equal to cost.
    let mut state:Call=decode(&h.svm,call);let denominator=math::U256::from(terms().scoring_window as u128)*math::U256::from(math::PRICE_SCALE);
    let numerator=math::U256::from(pos.cost)*denominator;let divisor=math::U256::from(pos.tokens);let integral=(numerator+divisor-math::U256::one())/divisor;
    state.price_integral=integral.low_u128();state.price_integral_hi=(integral>>128).low_u128();state.checkpointed=true;
    assert_eq!(state.value(pos.tokens,terms().scoring_window).unwrap(),pos.cost);assert!(!state.is_profitable(pos.tokens,pos.cost,terms().scoring_window).unwrap());
    let mut account=h.svm.get_account(&call).unwrap();state.try_serialize(&mut account.data.as_mut_slice()).unwrap();h.svm.set_account(call,account).unwrap();
    h.write_price_vector("breakEven",call,position,Some(integral));
    let caller_before=h.balance(caller.pubkey());let follower_before=h.balance(follower.pubkey());let settled=h.settle_call(call,position,caller.pubkey(),follower.pubkey()).unwrap();let outcome=call_settled(&settled);h.write_price_outcome("breakEven",&outcome);
    assert_eq!(outcome.exit_value,outcome.cost);assert!(!outcome.profitable);assert_eq!(h.balance(follower.pubkey())-follower_before,held);assert_eq!(h.balance(caller.pubkey()),caller_before);
    let wide=(math::U256::from(u64::MAX)+math::U256::one())*denominator/math::U256::from(2u8);state.price_integral=wide.low_u128();state.price_integral_hi=(wide>>128).low_u128();
    assert_eq!(state.value(2,terms().scoring_window).unwrap(),u64::MAX);assert!(state.is_profitable(2,u64::MAX,terms().scoring_window).unwrap());
}

#[test]
fn pathological_curve_trades_and_receipt_impact_saturates() {
    // Extreme fixture values exercise telemetry bounds without overflowing funds.
    let mut fixture=terms();fixture.total_supply=1_000_000_000_000;fixture.virtual_token=fixture.total_supply;fixture.curve_tokens=fixture.total_supply-1;fixture.virtual_sol=1;fixture.curve_fee_bps=0;fixture.pool_fee_bps=0;assert!(fixture.validate().is_ok());
    let mut h=Harness::with_terms(fixture);let trader=h.wallet();h.svm.airdrop(&trader.pubkey(),1_100_000_000_000).unwrap();let token=h.token_account(trader.pubkey());let gross=fixture.curve_tokens;
    let traded=h.trade("buyExactIn",gross,1,&trader,token,None).unwrap();let r=receipt(&traded,1);
    assert_eq!(r.price_impact_bps,i64::MAX);assert_eq!(h.m().phase,1);h.assert_sol_accounting();assert_eq!(h.token_balance(token),fixture.curve_tokens);
    let file=std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/chain-vectors.json");
    let mut vectors:Value=std::fs::read_to_string(&file).ok().and_then(|text|serde_json::from_str(&text).ok()).unwrap_or_else(||json!({"source":"LiteSVM executed SBF receipts","vectors":[]}));
    let mut vector=h.vectors.pop().unwrap();vector["feeTerms"]=json!({"curveFeeBps":0,"poolFeeBps":0,"protocolShareBps":5000,"creatorShareBps":3000,"poolShareBps":2000,"callShareBps":5000});
    vectors["vectors"].as_array_mut().unwrap().push(vector);std::fs::write(file,serde_json::to_string_pretty(&vectors).unwrap()).unwrap();
    assert_eq!(math::impact(u128::MAX,1).unwrap(),-9_999);assert_eq!(math::impact(1,u128::MAX).unwrap(),i64::MAX);
}

#[test]
fn creation_initializes_curve_with_fixed_supply_and_no_purchase() {
    let mut h=Harness::new();h.assert_sol_accounting();assert_eq!(h.m().phase,0);assert_eq!(h.m().real_sol,0);assert_eq!(h.m().creator_fees,0);
    assert_eq!(h.token_balance(h.vault),terms().total_supply);assert_eq!(decode::<Protocol>(&h.svm,h.protocol).accrued,0);assert!(h.m().oracle_exit_price>0);
    let mint_account=h.svm.get_account(&h.mint.pubkey()).unwrap();let mint=StateWithExtensions::<Mint>::unpack(&mint_account.data).unwrap();
    assert_eq!(mint.base.mint_authority,COption::None);assert_eq!(mint.base.freeze_authority,COption::None);assert_eq!(mint.base.supply,terms().total_supply);assert!(mint.get_extension_types().unwrap().len()>=2);
    let creator=h.creator.insecure_clone();let token=h.token_account(creator.pubkey());let traded=h.trade("buyExactIn",1_000_000,1,&creator,token,None).unwrap();assert!(receipt(&traded,1).tokens>0);
}

#[test]
fn create_and_buy_in_one_transaction_fails_atomically_for_every_buyer_and_mode() {
    for creator_buys in [true,false] {for exact_out in [false,true] {
        let mut h=Harness::initialized(terms());let creator=h.creator.insecure_clone();let mint=h.mint.insecure_clone();
        let other=h.wallet();let buyer=if creator_buys {&creator}else{&other};let token=Keypair::new();let rent:solana_sdk::rent::Rent=h.svm.get_sysvar();
        let receiver=solana_sdk::system_instruction::create_account(&h.payer.pubkey(),&token.pubkey(),rent.minimum_balance(TokenAccount::LEN),TokenAccount::LEN as u64,&spl_token_2022::ID);
        let initialize=spl_token_2022::instruction::initialize_account3(&spl_token_2022::ID,&token.pubkey(),&h.mint.pubkey(),&buyer.pubkey()).unwrap();
        let buy=if exact_out {h.trade_ix(instruction::BuyExactOut {tokens:1_000_000,max_gross:2_000_000},buyer.pubkey(),token.pubkey(),None)}
            else{h.trade_ix(instruction::BuyExactIn {gross:1_000_000,min_tokens:1},buyer.pubkey(),token.pubkey(),None)};
        let mut signers=vec![&creator,&mint,&token];if !creator_buys {signers.push(&other);}
        let before_creator=h.balance(creator.pubkey());let before_other=h.balance(other.pubkey());let protocol=h.svm.get_account(&h.protocol).unwrap();
        let refused=h.send(vec![h.create_ix(),receiver,initialize,buy],&signers).unwrap_err();assert!(refused.contains("BuyNotOpen"),"{refused}");
        for address in [h.market,h.mint.pubkey(),h.vault,h.history_address(),token.pubkey()] {assert!(h.svm.get_account(&address).is_none());}
        assert_eq!(h.balance(creator.pubkey()),before_creator);assert_eq!(h.balance(other.pubkey()),before_other);assert_eq!(h.svm.get_account(&h.protocol).unwrap(),protocol);
    }}
}

#[test]
fn same_slot_buys_fail_for_every_buyer_mode_and_tag_then_next_slot_matches_math() {
    let mut h=Harness::new();let created_slot=h.m().created_slot;let clock:Clock=h.svm.get_sysvar();assert_eq!(clock.slot,created_slot);
    let creator=h.creator.insecure_clone();let other=h.wallet();let caller=h.wallet();let ct=Keypair::new();let ot=Keypair::new();let rent:solana_sdk::rent::Rent=h.svm.get_sysvar();
    for (owner,token) in [(creator.pubkey(),&ct),(other.pubkey(),&ot)] {
        let allocate=solana_sdk::system_instruction::create_account(&h.payer.pubkey(),&token.pubkey(),rent.minimum_balance(TokenAccount::LEN),TokenAccount::LEN as u64,&spl_token_2022::ID);
        let initialize=spl_token_2022::instruction::initialize_account3(&spl_token_2022::ID,&token.pubkey(),&h.mint.pubkey(),&owner).unwrap();
        h.send_same_slot(vec![allocate,initialize],&[token]).unwrap();
    }
    let call=pda(&[b"call",h.market.as_ref(),caller.pubkey().as_ref(),&0u64.to_le_bytes()]);
    let open=ix(instruction::OpenCall {id:0,horizon_seconds:60},accounts::OpenCall {caller:caller.pubkey(),market:h.market,caller_sequence:pda(&[b"call-sequence",h.market.as_ref(),caller.pubkey().as_ref()]),call,system_program:solana_sdk::system_program::ID});
    h.send_same_slot(vec![open],&[&caller]).unwrap();
    let cp=pda(&[b"position",call.as_ref(),creator.pubkey().as_ref()]);let op=pda(&[b"position",call.as_ref(),other.pubkey().as_ref()]);
    for (follower,position) in [(&creator,cp),(&other,op)] {
        let prepare=ix(instruction::PreparePosition {},accounts::PreparePosition {payer:follower.pubkey(),follower:follower.pubkey(),market:h.market,call,position,system_program:solana_sdk::system_program::ID});
        h.send_same_slot(vec![prepare],&[follower]).unwrap();
    }
    let protected=[h.market,h.protocol,h.vault,h.mint.pubkey(),ct.pubkey(),ot.pubkey(),cp,op,creator.pubkey(),other.pubkey()];
    let before:Vec<_>=protected.iter().map(|key|h.svm.get_account(key)).collect();
    for (buyer,token,position) in [(&creator,ct.pubkey(),cp),(&other,ot.pubkey(),op)] {for tagged in [false,true] {for exact_out in [false,true] {
        let tag=if tagged {Some((call,position))}else{None};
        let buy=if exact_out {h.trade_ix(instruction::BuyExactOut {tokens:1_000_000,max_gross:2_000_000},buyer.pubkey(),token,tag)}
            else{h.trade_ix(instruction::BuyExactIn {gross:1_000_000,min_tokens:1},buyer.pubkey(),token,tag)};
        let refused=h.send_same_slot(vec![buy],&[buyer]).unwrap_err();assert!(refused.contains("BuyNotOpen"),"{refused}");
        let after:Vec<_>=protected.iter().map(|key|h.svm.get_account(key)).collect();assert_eq!(before,after);
    }}}
    h.advance_slot();let clock:Clock=h.svm.get_sysvar();assert_eq!(clock.slot,created_slot+1);
    for (buyer,token,position) in [(&creator,ct.pubkey(),cp),(&other,ot.pubkey(),op)] {for tagged in [false,true] {for exact_out in [false,true] {
        let before=h.m();let tag=if tagged {Some((call,position))}else{None};
        let (gross,output)=if exact_out {(math::gross_for_net(math::curve_buy_net(before.virtual_sol,before.virtual_token,1_000_000).unwrap(),terms().curve_fee_bps).unwrap(),1_000_000)}
            else{let fees=math::fees(1_000_000,terms().curve_fee_bps,&terms(),tagged).unwrap();(1_000_000,math::curve_buy_out(before.virtual_sol,before.virtual_token,1_000_000-fees.total).unwrap())};
        let fees=math::fees(gross,terms().curve_fee_bps,&terms(),tagged).unwrap();let wallet_before=h.balance(buyer.pubkey());let protocol_before=h.balance(h.protocol);
        let buy=if exact_out {h.trade_ix(instruction::BuyExactOut {tokens:output,max_gross:gross},buyer.pubkey(),token,tag)}
            else{h.trade_ix(instruction::BuyExactIn {gross,min_tokens:output},buyer.pubkey(),token,tag)};
        let executed=h.send_same_slot(vec![buy],&[buyer]).unwrap();let r=receipt(&executed,1);let after=h.m();
        assert_eq!((r.gross,r.tokens,r.protocol_fee,r.creator_fee,r.call_fee,r.pool_fee),(gross,output,fees.protocol,fees.creator,fees.call,fees.pool));
        assert_eq!(wallet_before-h.balance(buyer.pubkey()),gross);assert_eq!(h.balance(h.protocol)-protocol_before,fees.protocol);
        assert_eq!(after.real_sol-before.real_sol,r.net+fees.pool);assert_eq!(after.creator_fees-before.creator_fees,fees.creator);assert_eq!(after.created_slot,created_slot);h.assert_sol_accounting();
    }}}
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
    let mut h=Harness::new();let trader=h.wallet();let caller=h.wallet();let token=h.token_account(trader.pubkey());let call=h.open_call(&caller,0,60);let position=h.position(call,&trader);
    let protocol_before=h.balance(h.protocol);let wallet_before=h.balance(trader.pubkey());let m0=h.m();
    let meta=h.trade("buyExactIn",10_000_003,1,&trader,token,Some((call,position))).unwrap();let r=receipt(&meta,1);let m1=h.m();let pos:CallPosition=decode(&h.svm,position);
    assert_eq!(r.gross,r.net+r.protocol_fee+r.creator_fee+r.call_fee+r.pool_fee);assert_eq!(wallet_before-h.balance(trader.pubkey()),r.gross);assert_eq!(h.balance(h.protocol)-protocol_before,r.protocol_fee);assert_eq!(m1.creator_fees-m0.creator_fees,r.creator_fee);assert_eq!(pos.escrow,r.call_fee);assert_eq!(m1.real_sol-m0.real_sol,r.net+r.pool_fee);
    let untagged=math::fees(r.gross,terms().curve_fee_bps,&terms(),false).unwrap();assert_eq!(untagged.total,r.protocol_fee+r.creator_fee+r.call_fee+r.pool_fee);assert_eq!(untagged.protocol,r.protocol_fee+r.call_fee);
    h.trade("buyExactOut",1_000_003,u64::MAX,&trader,token,None).unwrap();h.trade("sellExactIn",100_003,0,&trader,token,None).unwrap();h.trade("sellExactOut",10_003,u64::MAX,&trader,token,None).unwrap();
    let before=h.m();let output=before.curve_remaining;let meta=h.trade("buyExactOut",output,u64::MAX,&trader,token,None).unwrap();let after=h.m();assert_eq!(after.phase,1);assert!(after.burned_tokens>0);assert_eq!(math::spot(after.real_sol,after.pool_token_num,after.pool_token_den).unwrap(),math::spot(after.virtual_sol,after.virtual_token as u128,1).unwrap());assert!(meta.logs.iter().any(|l|l.starts_with("Program data: ")));
    h.trade("buyExactIn",1_000_003,1,&trader,token,None).unwrap();h.trade("buyExactOut",100_003,u64::MAX,&trader,token,None).unwrap();h.trade("sellExactIn",100_003,0,&trader,token,None).unwrap();h.trade("sellExactOut",10_003,u64::MAX,&trader,token,None).unwrap();
    let creator_signer=h.creator.insecure_clone();let before=h.balance(creator_signer.pubkey());let fees=h.m().creator_fees;let claim=ix(instruction::ClaimCreatorFees {},accounts::ClaimCreatorFees {creator:creator_signer.pubkey(),market:h.market});h.send(vec![claim.clone()],&[&creator_signer]).unwrap();assert_eq!(h.balance(creator_signer.pubkey())-before,fees);h.send(vec![claim],&[&creator_signer]).unwrap();assert_eq!(h.balance(creator_signer.pubkey())-before,fees);h.assert_sol_accounting();
    let mut new_terms=terms();new_terms.pool_fee_bps=9999;let update=ix(instruction::UpdateConfig {terms:new_terms,paused:true},accounts::AdminConfig {admin:creator_signer.pubkey(),config:h.config});h.send(vec![update],&[&creator_signer]).unwrap();assert_eq!(h.m().terms.pool_fee_bps,terms().pool_fee_bps);h.trade("buyExactIn",1_000_003,1,&trader,token,None).unwrap();h.write_vectors();
}

#[test]
fn call_profit_pays_caller_and_settlement_cannot_pay_twice() {
    let mut h=Harness::new();let caller=h.wallet();let follower=h.wallet();let whale=h.wallet();let ft=h.token_account(follower.pubkey());let wt=h.token_account(whale.pubkey());let c=h.open_call(&caller,0,60);let p=h.position(c,&follower);
    h.trade("buyExactIn",1_000_000,1,&follower,ft,Some((c,p))).unwrap();h.trade("buyExactIn",80_000_000,1,&whale,wt,None).unwrap();let pos:CallPosition=decode(&h.svm,p);let before=h.balance(caller.pubkey());let follower_before=h.balance(follower.pubkey());let rent_back=h.balance(p)-pos.escrow;h.mature(c);let settled=h.settle_call(c,p,caller.pubkey(),follower.pubkey()).unwrap();let scored=call_settled(&settled);assert_eq!(h.balance(p),0);assert_eq!(h.balance(follower.pubkey())-follower_before,rent_back);assert!(scored.profitable);assert!(scored.exit_value>scored.cost);assert_eq!(h.balance(caller.pubkey())-before,pos.escrow);let after=h.balance(caller.pubkey());let _=h.settle_call(c,p,caller.pubkey(),follower.pubkey());assert_eq!(h.balance(caller.pubkey()),after);
}

#[test]
fn call_loss_refunds_follower_and_last_second_pump_does_not_flip_it() {
    let mut h=Harness::new();let caller=h.wallet();let follower=h.wallet();let whale=h.wallet();let ft=h.token_account(follower.pubkey());let wt=h.token_account(whale.pubkey());
    h.trade("buyExactIn",80_000_000,1,&whale,wt,None).unwrap();let c=h.open_call(&caller,0,60);let p=h.position(c,&follower);h.trade("buyExactIn",10_000_000,1,&follower,ft,Some((c,p))).unwrap();let all=h.token_balance(wt);h.trade("sellExactIn",all,0,&whale,wt,None).unwrap();let horizon=decode::<Call>(&h.svm,c).horizon;h.time(horizon-1);h.trade("buyExactIn",1_000_000_000,1,&whale,wt,None).unwrap();let pos:CallPosition=decode(&h.svm,p);assert!((pos.tokens as u128)*h.m().exit_price().unwrap()/math::PRICE_SCALE>pos.cost as u128,"fixture must show spot-only scoring could be fooled");
    let before=h.balance(follower.pubkey());let rent_back=h.balance(p)-pos.escrow;h.time(horizon+70);let settled=h.settle_call(c,p,caller.pubkey(),follower.pubkey()).unwrap();let scored=call_settled(&settled);assert_eq!(h.balance(p),0);assert!(!scored.profitable);assert!(scored.exit_value<scored.cost);assert_eq!(h.balance(follower.pubkey())-before,pos.escrow+rent_back);let call:Call=decode(&h.svm,c);assert_eq!(call.refunded,pos.escrow);
}

#[test]
fn unfunded_caller_does_not_block_follower_rent_and_deferred_payouts_are_claimable() {
    let mut h=Harness::new();let caller=h.wallet();let follower=h.wallet();let whale=h.wallet();
    let ft=h.token_account(follower.pubkey());let wt=h.token_account(whale.pubkey());
    let first=h.open_call(&caller,0,60);let second=h.open_call(&caller,1,60);
    let first_rent=h.balance(first);let second_rent=h.balance(second);
    let fp=h.position(first,&follower);let sp=h.position(second,&follower);
    h.trade("buyExactIn",1_000_000,1,&follower,ft,Some((first,fp))).unwrap();
    h.trade("buyExactIn",1_000_000,1,&follower,ft,Some((second,sp))).unwrap();
    h.trade("buyExactIn",80_000_000,1,&whale,wt,None).unwrap();
    let first_position:CallPosition=decode(&h.svm,fp);let second_position:CallPosition=decode(&h.svm,sp);
    let follower_rent=h.balance(fp)+h.balance(sp)-first_position.escrow-second_position.escrow;
    let drain=solana_sdk::system_instruction::transfer(&caller.pubkey(),&h.payer.pubkey(),h.balance(caller.pubkey()));
    h.send(vec![drain],&[&caller]).unwrap();assert_eq!(h.balance(caller.pubkey()),0);
    let follower_before=h.balance(follower.pubkey());h.mature(first);
    for (call,position) in [(first,fp),(second,sp)] {
        let settled=h.settle_call(call,position,caller.pubkey(),follower.pubkey()).unwrap();
        let event=call_settled(&settled);assert!(event.profitable);assert!(event.payout_deferred);
        assert_eq!(h.balance(position),0);
    }
    assert_eq!(h.balance(follower.pubkey())-follower_before,follower_rent);
    assert_eq!(h.balance(caller.pubkey()),0);
    let first_state:Call=decode(&h.svm,first);let second_state:Call=decode(&h.svm,second);
    assert_eq!(first_state.claimable_payout,first_position.escrow);
    assert_eq!(second_state.claimable_payout,second_position.escrow);
    assert_eq!(h.balance(first),first_rent+first_position.escrow);
    assert_eq!(h.balance(second),second_rent+second_position.escrow);
    let claim=ix(instruction::ClaimCallPayout {},accounts::ClaimCallPayout {settler:h.payer.pubkey(),market:h.market,call:second,caller:caller.pubkey()});
    let refusal=h.send(vec![claim.clone()],&[]).unwrap_err();assert!(refusal.contains("RecipientNeedsRent"),"{refusal}");
    assert_eq!(decode::<Call>(&h.svm,second).claimable_payout,second_position.escrow);
    let close=ix(instruction::CloseCall {},accounts::CloseCall {caller:caller.pubkey(),call:first});
    h.send(vec![close],&[&caller]).unwrap();assert_eq!(h.balance(first),0);
    assert_eq!(h.balance(caller.pubkey()),first_rent+first_position.escrow);
    let before=h.balance(caller.pubkey());let paid=h.send(vec![claim.clone()],&[]).unwrap();
    assert_eq!(receipt(&paid,6).net,second_position.escrow);
    assert_eq!(h.balance(caller.pubkey())-before,second_position.escrow);
    assert_eq!(decode::<Call>(&h.svm,second).claimable_payout,0);
    assert_eq!(h.balance(second),second_rent);
    let before=h.balance(caller.pubkey());h.send(vec![claim],&[]).unwrap();assert_eq!(h.balance(caller.pubkey()),before);
    let close=ix(instruction::CloseCall {},accounts::CloseCall {caller:caller.pubkey(),call:second});
    h.send(vec![close],&[&caller]).unwrap();assert_eq!(h.balance(second),0);
    assert_eq!(h.balance(caller.pubkey())-before,second_rent);
}

#[test]
fn math_splits_exactly_and_inverse_fee_handles_u64_edge() {
    for gross in [0,1,9,999,10_001,1_000_000_003,u64::MAX] { let a=math::fees(gross,333,&terms(),true).unwrap();let b=math::fees(gross,333,&terms(),false).unwrap();assert_eq!(a.total,a.protocol+a.creator+a.call+a.pool);assert_eq!(a.total,b.total);assert_eq!(a.protocol+a.call,b.protocol); }
    for rate in [0,1,333,9_999] {let net=u64::MAX-((u64::MAX as u128*rate as u128/10_000) as u64);let gross=math::gross_for_net(net,rate).unwrap();assert_eq!(gross-math::mul_div(gross,rate as u64,10_000).unwrap(),net);}
}


#[test]
fn minute_edges_reconstruct_across_quiet_minutes_and_intraminute_price_changes() {
    let mut h=Harness::new();let caller=h.wallet();let follower=h.wallet();let whale=h.wallet();
    let ft=h.token_account(follower.pubkey());let wt=h.token_account(whale.pubkey());
    let call=h.open_call(&caller,0,300);let position=h.position(call,&follower);
    let timing:Call=decode(&h.svm,call);assert_eq!(timing.horizon%60,0);assert_eq!(timing.window_start%60,0);
    assert!(timing.horizon-timing.opened_at>=terms().min_horizon && timing.horizon-timing.opened_at<=terms().max_horizon);
    h.trade("buyExactIn",1_000_000,1,&follower,ft,Some((call,position))).unwrap();
    h.trade("buyExactIn",80_000_000,1,&whale,wt,None).unwrap();
    h.time(1_045);h.trade("buyExactIn",10_000_000,1,&whale,wt,None).unwrap();
    h.time(1_075);h.trade("sellExactIn",100_000,0,&whale,wt,None).unwrap();
    let previous=h.m();assert!(timing.window_start>previous.oracle_timestamp+60);
    let expected=math::U256::from(previous.oracle_exit_price)*math::U256::from(terms().scoring_window as u128);
    let refusal=h.checkpoint(call).unwrap_err();assert!(refusal.contains("CallNotMature"),"{refusal}");
    // Both scoring boundaries occur in minutes with no trades. A later trade
    // materializes their exact integrals, including the earlier intraminute moves.
    h.time(timing.horizon+127);h.trade("buyExactIn",50_000,1,&whale,wt,None).unwrap();
    h.write_price_vector("exact",call,position,Some(expected));
    h.checkpoint(call).unwrap();let scored:Call=decode(&h.svm,call);
    assert!(scored.checkpointed);assert!(!scored.evidence_missing);assert_eq!(scored.integral(),expected);
    let settled=h.settle_call(call,position,caller.pubkey(),follower.pubkey()).unwrap();
    let outcome=call_settled(&settled);h.write_price_outcome("exact",&outcome);assert!(outcome.profitable);assert_eq!(h.balance(position),0);
}

#[test]
fn checkpoint_survives_ring_overwrite_and_late_followers() {
    let mut h=Harness::new();let caller=h.wallet();let first=h.wallet();let second=h.wallet();let whale=h.wallet();
    let ft=h.token_account(first.pubkey());let st=h.token_account(second.pubkey());let wt=h.token_account(whale.pubkey());
    let call=h.open_call(&caller,0,60);let fp=h.position(call,&first);let sp=h.position(call,&second);
    h.trade("buyExactIn",1_000_000,1,&first,ft,Some((call,fp))).unwrap();
    h.trade("buyExactIn",1_000_000,1,&second,st,Some((call,sp))).unwrap();
    h.trade("buyExactIn",80_000_000,1,&whale,wt,None).unwrap();h.mature(call);
    h.checkpoint(call).unwrap();let frozen:Call=decode(&h.svm,call);assert!(frozen.checkpointed);
    assert!(!frozen.evidence_missing);
    let second_position:CallPosition=decode(&h.svm,sp);let expected=frozen.value(second_position.tokens,terms().scoring_window).unwrap();
    let first_meta=h.settle_call(call,fp,caller.pubkey(),first.pubkey()).unwrap();assert!(call_settled(&first_meta).profitable);
    let history_before=h.svm.get_account(&h.history_address()).unwrap();
    let accounts_before=h.trade_ix(instruction::BuyExactIn {gross:50_000,min_tokens:1},whale.pubkey(),wt,None).accounts;
    h.time(frozen.horizon+3*24*60*60);
    let traded=h.trade("buyExactIn",50_000,1,&whale,wt,None).unwrap();assert!(traded.compute_units_consumed<1_400_000);
    let accounts_after=h.trade_ix(instruction::BuyExactIn {gross:50_000,min_tokens:1},whale.pubkey(),wt,None).accounts;
    assert_eq!(accounts_before,accounts_after);
    let history_after=h.svm.get_account(&h.history_address()).unwrap();
    assert_eq!(history_after.data.len(),9_704);assert_eq!(history_before.data.len(),history_after.data.len());assert_eq!(history_before.lamports,history_after.lamports);
    // Even a repeated, very late checkpoint cannot overwrite the frozen edges.
    h.checkpoint(call).unwrap();let still_frozen:Call=decode(&h.svm,call);assert_eq!(frozen.integral(),still_frozen.integral());
    let late=h.settle_call(call,sp,caller.pubkey(),second.pubkey()).unwrap();let outcome=call_settled(&late);assert_eq!(outcome.exit_value,expected);assert!(outcome.profitable);assert_eq!(h.balance(sp),0);
}

#[test]
fn overwritten_uncheckpointed_window_refunds_follower_without_waiting() {
    let mut h=Harness::new();let caller=h.wallet();let follower=h.wallet();let whale=h.wallet();
    let ft=h.token_account(follower.pubkey());let wt=h.token_account(whale.pubkey());let call=h.open_call(&caller,0,60);let position=h.position(call,&follower);
    h.trade("buyExactIn",1_000_000,1,&follower,ft,Some((call,position))).unwrap();
    h.trade("buyExactIn",80_000_000,1,&whale,wt,None).unwrap();
    let timing:Call=decode(&h.svm,call);let held=h.balance(position);let before=h.balance(follower.pubkey());let caller_before=h.balance(caller.pubkey());
    // No keeper or checkpoint runs. The first trade after a gap greater than
    // retention must succeed using exactly the account allocated at creation.
    h.time(timing.horizon+3*24*60*60);let trade=h.trade("buyExactIn",50_000,1,&whale,wt,None).unwrap();assert!(trade.compute_units_consumed<1_400_000);
    h.write_price_vector("expired",call,position,None);
    let settled=h.settle_call(call,position,caller.pubkey(),follower.pubkey()).unwrap();let outcome=call_settled(&settled);
    h.write_price_outcome("expired",&outcome);
    assert!(!outcome.profitable);assert_eq!(outcome.exit_value,0);assert_eq!(h.balance(position),0);
    assert_eq!(h.balance(follower.pubkey())-before,held);assert_eq!(h.balance(caller.pubkey()),caller_before);
    let frozen:Call=decode(&h.svm,call);assert!(frozen.checkpointed && frozen.evidence_missing);assert_eq!(frozen.refunded,outcome.payout);
    h.checkpoint(call).unwrap();assert!(decode::<Call>(&h.svm,call).evidence_missing);
}

#[test]
fn more_than_sixteen_calls_add_no_accounts_to_a_trade() {
    let mut h=Harness::new();let caller=h.wallet();let trader=h.wallet();let token=h.token_account(trader.pubkey());
    let before=h.trade_ix(instruction::BuyExactIn {gross:1_000_000,min_tokens:1},trader.pubkey(),token,None);
    let mut calls=Vec::new();for id in 0..24 {calls.push(h.open_call(&caller,id,60));}
    let after=h.trade_ix(instruction::BuyExactIn {gross:1_000_000,min_tokens:1},trader.pubkey(),token,None);
    assert_eq!(before.accounts,after.accounts);assert_eq!(decode::<CallerSequence>(&h.svm,pda(&[b"call-sequence",h.market.as_ref(),caller.pubkey().as_ref()])).next_id,24);
    assert!(calls.iter().all(|call|after.accounts.iter().all(|account|account.pubkey!=*call)));
    h.trade("buyExactIn",1_000_000,1,&trader,token,None).unwrap();h.mature(calls[0]);
    for call in calls {let rent=h.balance(call);let before=h.balance(caller.pubkey());let ins=ix(instruction::CloseCall {},accounts::CloseCall {caller:caller.pubkey(),call});h.send(vec![ins],&[&caller]).unwrap();assert_eq!(h.balance(caller.pubkey())-before,rent);assert_eq!(h.balance(call),0);}
}

#[test]
fn unused_position_and_completed_call_return_their_rent() {
    let mut h=Harness::new();let caller=h.wallet();let follower=h.wallet();let call=h.open_call(&caller,0,60);
    let before=h.balance(follower.pubkey());let position=h.position(call,&follower);let rent=h.balance(position);assert_eq!(before-h.balance(follower.pubkey()),rent);
    let close=ix(instruction::ClosePosition {},accounts::ClosePosition {follower:follower.pubkey(),call,position});h.send(vec![close],&[&follower]).unwrap();assert_eq!(h.balance(position),0);assert_eq!(h.balance(follower.pubkey()),before);
    let state:Call=decode(&h.svm,call);assert_eq!(state.position_count,state.settled_positions);assert_eq!(state.follower_count,0);
    h.mature(call);let rent=h.balance(call);let before=h.balance(caller.pubkey());let close=ix(instruction::CloseCall {},accounts::CloseCall {caller:caller.pubkey(),call});h.send(vec![close],&[&caller]).unwrap();assert_eq!(h.balance(call),0);assert_eq!(h.balance(caller.pubkey())-before,rent);
    let replay=ix(instruction::OpenCall {id:0,horizon_seconds:60},accounts::OpenCall {caller:caller.pubkey(),market:h.market,caller_sequence:pda(&[b"call-sequence",h.market.as_ref(),caller.pubkey().as_ref()]),call,system_program:solana_sdk::system_program::ID});assert!(h.send(vec![replay],&[&caller]).is_err());assert_eq!(h.balance(call),0);
}

