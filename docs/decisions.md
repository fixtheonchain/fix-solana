# Protocol decisions

Each coin starts on its curve and accepts buys from the slot after its creation. Economic values come from the
initialized on-chain terms.

## Creation and money

Amounts are integer token atoms and lamports. One total fee is charged; protocol and pool shares round down, creator receives the remainder, and the tagged call slice comes out of protocol. Following never increases the trading fee.

Creation makes the Token-2022 mint with on-mint metadata and sends the full fixed supply to the market. There is no creator allocation, remaining mint authority or freeze authority. It records immutable `created_slot`. Every buy requires `Clock.slot > created_slot`, otherwise returning `BuyNotOpen`. The shared handler applies this before charging or changing reserves, including exact-in/out and tagged buys. This prevents creation-transaction buys, bundles and separate same-slot buys, for the creator and every other wallet. Buying opens at `created_slot + 1` with no configurable delay, keeper or added sell restriction. Creation costs rent only; a later purchase pays the ordinary immutable curve fee.

A capped curve buy leaves unused input budget with the buyer, and its quote reports that amount separately from the accepted all-in debit.

## Graduation and authority

The last curve buy graduates in the same instruction, with no new accounts, migration fee, LP token or withdrawal path. Rational effective pool reserves preserve the exact final curve price. Physical backing rounds up by less than one atom and excess whole atoms burn.

Config updates and pause affect future creation only. Creation checks the preview revision. Initialization checks program upgrade authority; admin transfer cannot reassign creators or unlock funds. SOL protocol fees accrue to the PDA seeded `protocol`, without a withdrawal instruction.

## Fixed market price record

The market keeps fee-net marginal Q64 price and a U256 cumulative integral. Trades contain no Call accounts. Tagged buys authenticate cached caller/horizon data from their follower position; separate registration reads the call. Calls remain unbounded.

One creator-funded 9,704-byte zero-copy `PriceHistory` holds 241 exact minute-boundary integrals, covering four hours between retained edges under continuous trading. Its size fits the 10,240-byte inner-instruction allocation limit. Reserve changes materialize crossed boundaries using the previous price before installing the new price. A long quiet gap writes only the latest 241 boundaries. Within a minute, only the global accumulator changes. Trades never allocate, request prepared storage, or depend on a keeper.

Windows and horizons align to whole minutes. The requested horizon is rounded down to a boundary, then advanced one minute if needed to meet the minimum actual duration. Both actual bounds are checked. Windows must be positive multiples of 60 and no longer than 7,200 seconds, leaving another 7,200 seconds of guaranteed checkpoint margin inside the ring's 14,400-second span. Horizon bounds need at least 59 seconds of width so alignment is possible at any clock second.

Anyone checkpoints a completed window from the fixed history, or settlement checkpoints implicitly. Historical boundaries are exact ring values; a quiet tail extrapolates exactly from the last reserve change. A successful checkpoint freezes the integral forever. If an edge has been overwritten first, checkpointing freezes `evidence_missing` and settlement refunds the follower. This outcome is recorded separately from a scored loss. A current-price estimate is never used for missing history. Keepers are a convenience; a caller benefits from preserving the evidence near its horizon.

Scoring uses time-weighted marginal exit price net of the phase sell fee, multiplied by tagged tokens. The unsaturated integer mark must strictly exceed tagged cost; equality refunds the follower. Portfolio exit quotes also include whole-position impact. Later sells/transfers do not rewrite hypothetical tagged allocations. Values above u64 saturate for reporting only; the profit comparison retains their precision. Receipt impact is a saturated signed i64 metric, so extreme ratios cannot revert a money operation. A last-second pump cannot rewrite prior time.

Each caller/market pair owns a persistent monotonic CallerSequence, preventing closed addresses and old share links from being reused without allowing other callers to race its IDs.

## Rent

There is no sponsor. Users pay follower-position rent. Settlement pays the slice and closes the follower position to its user. Empty unused positions have an owner close path. Callers close calls after the horizon and all registered positions finish. Token ATAs holding tokens remain usable token accounts rather than spent protocol positions.

An unfunded caller cannot receive a tiny SOL credit below the System account rent floor. A profitable settlement holds such a payout on the existing call account instead of blocking the follower's rent return. A permissionless claim transfers the accrued payout once its destination can accept it; closing the completed call returns rent plus any held payout. The claimable payout is a liability separate from the call's refundable rent. No new account is allocated.

## Events

The program emits its receipts with `emit!` only, with no inner-instruction events. A decoder tracks runtime invoke depth and completion and reads only the `Program data:` lines emitted while fix is executing; Anchor instruction messages and other programs' logs are diagnostics.
