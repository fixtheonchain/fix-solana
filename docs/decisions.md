# Protocol decisions

The launchpad's decisions come first; the fix AMM's are in the last section.

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

## The fix AMM

`programs/fix-amm` holds pools for tokens that were not born on the launchpad: any SPL Token or Token-2022 mint against
SOL, one pool per mint.

- **The seeding slot is closed to swaps.** A pool's first liquidity sets its price. Buys and sells require
  `Clock.slot > seeded_slot`, for every wallet, so nobody holds a pool's price before the rest of the market can see
  the pool. An emptied pool seeded again sets `seeded_slot` anew. Adds and removes are not swaps and stay open.
- **Terms in the pool.** `create_pool` copies the config's terms and checks `expected_config_revision`. No instruction
  changes a live pool's terms, reserves or positions on anyone's behalf.
- **The fee.** `fee_bps` of the SOL side of each swap, `total = gross * fee_bps / 10_000` rounded down. The protocol's
  part is `total * protocol_share_bps / 10_000` rounded down and goes to the named shard in the same instruction; the
  liquidity providers' part is the remainder and stays in the SOL reserve. A buy adds `gross - protocol` to the SOL
  reserve and pays `y * net / (x + net)` tokens; a sell takes `net + protocol` out of it for `x * input / (y + input)`
  of gross value. Exact-out modes solve the smallest gross with the same fee rounding. There is no creator line: being
  first to open a pool is position, not action.
- **Shares.** A first deposit into an empty pool mints `floor(sqrt(sol * tokens))` and takes both amounts whole. Later
  adds mint exactly the shares asked for and take `ceil(shares * reserve / total)` of each side; removes pay
  `floor(shares * reserve / total)`. Shares never exceed `sqrt(x * y)`, so they fit a u64. A pool whose shares are
  each worth a lot costs a depositor at most one atom of each side, so no minimum liquidity is burned or locked; every
  share stays withdrawable by its owner.
- **Reserves in the account.** The SOL reserve is the pool's lamports above rent and the token reserve its vault
  balance, both tracked as fields, so donations move no price. Lamports leave the pool by direct arithmetic, never below
  its rent floor.
- **Mints.** Wrapped SOL in either program is refused. A Token-2022 mint's TLV entries must all be in the allow list:
  interest-bearing (10), metadata pointer (18), metadata (19), group pointer (20), group (21), member pointer (22),
  member (23) and scaled UI amount (25). Transfer fees, hooks, permanent delegates, default account state, pausing,
  confidential transfers, close authorities and any later type would let a transfer or the vault change outside the
  pool, so a quote could differ from execution.
- **Launchpad coins.** `create_pool` derives the launchpad's `["market", mint]` address and refuses a mint whose market
  exists there (`LaunchpadCoin`). A launchpad coin trades under the terms sealed in its own market from its first trade
  to its last; a second venue with other terms would break that.
- **Events.** `PoolCreated` and `Receipt` through `#[event_cpi]`. A receipt's action is 0 create, 1 buy, 2 sell, 3 add,
  4 remove, and it carries the reserves, the shares outstanding and the price after the action.
