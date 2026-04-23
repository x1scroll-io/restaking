# x1scroll Restaking

**EigenLayer-style restaking for X1. The first native restaking protocol on any SVM chain.**

Validators restake XNT to secure external protocols — bridges, oracles, DA layers, rollups. Earn additional rewards. x1scroll takes 10% platform fee (50% treasury / 50% burned 🔥).

## How It Works

```
Validator bonds XNT
    → opts into AVS (bridge, oracle, rollup)
    → secures that protocol with staked collateral
    → earns reward_rate_bps on top of normal staking yield
    → misbehave → 20% slashed
```

## For AVS Protocols

Register your protocol as an Actively Validated Service:

```
register_avs(
  name: "My Bridge",
  min_operator_stake: 100 XNT,
  reward_rate_bps: 500,  // 5% APY to operators
  registration_fee: X XNT
)
```

## Fee Structure (immutable)

| Fee | Amount | Split |
|-----|--------|-------|
| AVS registration | Variable | 50% treasury / 50% burned |
| Platform fee on rewards | 10% | 50% treasury / 50% burned |
| Slash penalty | 20% of stake | Goes to treasury + burned |
| Unbond cooldown | 14 epochs | — |

## Program ID
`9qoVHkGeZEnrFC7rZi3tLxRafT2HXwCQ3nGueEGcUdtN` — live on X1 mainnet

## Why This Is Huge
EigenLayer raised $100M+ on Ethereum for this exact primitive. X1 has nothing like it. x1scroll ships it first, owns the category.

Built by x1scroll.io | @ArnettX1
