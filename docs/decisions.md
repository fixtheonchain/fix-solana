# Protocol decisions

The program holds coin creation, curve and pool trading, fixed fees, creator claims, admin configuration and the
extension point. Features beyond these ship as extensions; calls arrive as the first one, with no core change. The
core holds no call instructions, accounts, terms, tags or price records, and reserves none in its account layout.

## Creation and money

Amounts are integer token atoms and lamports. One fee is charged. Protocol and pool shares round down, and the
creator receives the remainder. Supply, curve reserves and fees are copied from config at creation and never change
for that coin.

Creation accepts only mint public keys whose base58 address ends in lowercase `fix`. It makes a Token-2022 mint with
on-mint metadata and sends the full fixed supply to its market vault, leaving no creator allocation, mint authority or
freeze authority. The only creation cost is rent for the mint, market and vault. Market and Config allocations are 214
and 125 bytes; a protocol shard is 17 bytes.

Market records an immutable `created_slot`. Every exact-in and exact-out buy requires `Clock.slot > created_slot`,
otherwise `BuyNotOpen`; no funds move before this guard. This refuses same-transaction, bundled and separate
creation-slot buys for every wallet. Buying opens at `created_slot + 1` without a configurable delay. Sells keep the
ordinary reserve checks.

A capped curve buy leaves the unused input budget with the buyer, and the quote reports it separately from the
accepted all-in debit. Receipt price impact saturates as a signed i64 display metric, so extreme ratios cannot revert
a valid money operation.

## Graduation and authority

The last curve buy graduates in the same instruction with no extra accounts or rent, migration fee, LP token or
withdrawal path. Rational effective pool reserves preserve the final curve price. Physical backing rounds up by less
than one atom, and excess whole atoms burn. Liquidity stays locked in the program.

Config updates and pause affect future creation only. Creation checks the revision the creator previewed
(`ConfigChanged`). Initialization checks the program upgrade authority; an admin transfer cannot reassign creators or
unlock funds. Protocol SOL accrues above rent to the 64 PDAs seeded `["protocol", shard]`, credited in the instruction
that charges the fee, with no withdrawal instruction. A trade writes its market and one shard, so markets share no
write lock. Anyone may create a shard.

`scripts/mainnet-terms.json` holds the launch terms: curve and pool fees of 100 bps, protocol, creator and pool shares
of 4000, 4000 and 2000 bps, a supply of 10^15 atoms at 6 decimals, virtual reserves of 30,000,000,000 lamports and
10^15 atoms, and 750,000,000,000,000 atoms for sale on the curve.

## Extensions

Extensions are separate programs that call the core's buy and sell instructions by CPI; the core never calls out to
them. The admin registers, pauses and removes an extension without a core upgrade. One `["extension", program_id]` PDA
holds the extension's program, its signing PDA (`["fix-extension"]` under that program), its share recipient and its
share of the protocol part in bps of that part.

A routed trade passes exactly three remaining accounts: the entry, the signing PDA as signer and the recipient. Only
the extension program can produce that signature, so the trade is proven to come from it. Any other number of
remaining accounts, an unregistered or paused entry, an unsigned or different authority and a different recipient are
refused by name (6012–6017).

The core pays the share out of the protocol part in the same instruction, rounded down, and the protocol shard keeps
the rest. The trader's cost, quotes, creator and pool lines and the coin's terms are those of a plain trade. Receipts
carry `extension` and `extension_fee`, and `protocol_fee + extension_fee` is the plain protocol line. Plain trades never
read the registry, so pausing or removing an entry stops only new routed trades; removing one closes its entry and
returns the rent to the admin.

## Events

Events use Anchor `#[event_cpi]`: each is a self-CPI signed by `["__event_authority"]`, so it travels as instruction
data that the runtime records in full instead of in the 10,000-byte log budget. A decoder reads only fix instructions
that carry the event tag with the event authority as their account. A transaction whose logs end in `Log truncated`
still carries every receipt.

## Vanity mints

The suffix guard folds the 32 public-key bytes big-endian modulo 58^3 = 195,112 and requires 130,265, the base58
value 38·58^2 + 41·58 + 55 of lowercase `fix`. `InvalidMintSuffix` is 6010. The tests compare the derivation with real
base58 encodings and run accepted and refused creation. A feature-gated build (`measure-mint-suffix`) measures only the
guard and subtracts the overhead of an adjacent empty probe; normal builds include no profiling code. The guard
measures 312 CU.

## Rent

Users pay rent for their own token accounts, which stay theirs to close. Creation pays rent for the mint, market and
vault. An extension's registry entry is paid by the admin and returns to the admin on removal.
