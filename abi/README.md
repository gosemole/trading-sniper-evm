# ABIs

Reference copies, checked in as they came off the deployed contracts. Nothing
here is compiled or loaded at runtime - the bot builds its own selectors and
topics by hand, the way the rest of the tree does - but `launch.rs` reads these
in a test, so a signature drifting is a failing test rather than a subscription
that quietly hears nothing.

| file | address | |
|---|---|---|
| `PonsV2LaunchFactory.json` | `0x7eD598BcEf8bd9Edd8C97A195C6d13f40801EC7e` | mints the token and opens its bonding curve. Emits `TokenLaunched`, the one log every launch has |
| `PonsV2LaunchAndBuy.json` | `0xe33E9E479dF8802cb0866d5d05258bEc4cF62948` | `launchAndBuy`: calls the factory and buys from the curve in one transaction. Emits `Launched`, the dev buy |
| `PonsV2BondingCurve.json` | one per launch | what a buy actually calls. `buy(quoteIn, minTokensOut, recipient)`, the reserves it prices against, and its own copy of the snipe tax |

The curve is where the money is, and three things in its ABI decide how a buy
is made:

- `buy(uint256 quoteIn, uint256 minTokensOut, address recipient) payable` -
  `minTokensOut` is the only slippage protection there is.
- `getReserves()` with `phantomQuote`, `feeBps` and `creatorTaxBps`: enough to
  price a trade locally, which `curve.rs` does. The reserves are all a
  constant-product curve needs, and the quote side is virtual until somebody
  buys.
- `CurveBuy(buyer, recipient, quoteIn, tokensOut, fee, tax)` - **the tax
  actually charged, in the log of every buy**. That is the one thing that can
  ever prove the local model right, and it costs nothing to read.

The snipe tax lives on the curve, not the factory: `snipeTaxStartBps`,
`snipeTaxSeconds` and `launchedAt` are copied into it at launch, and
`currentSnipeTaxBps(recipient)` applies them per buyer. So the factory's
settings say what the NEXT launch will be taxed at, and each curve keeps the
snapshot its own buyers pay.

Worth knowing, from the factory's ABI rather than from watching:

- `pairTokenEconomics(pairToken)` returns `phantomQuote`, `graduationThreshold`
  and **`decimals`** - so the pair token's decimals can be had from the factory
  itself, without asking the token.
- `getLaunchedToken(token)` returns the whole launch in one call: curve,
  deployer, pair token, graduation threshold, pool fee, tick spacing,
  `creatorTaxBps` and the graduation phase.
- `getLaunchConfig(id)` is what the `launchConfigId` in a launch log points at:
  supply, `curveFeeBps`, `phantomQuote`, `graduationThreshold`, pool fee, tick
  spacing.
- `snipeTaxStartBps` and `snipeTaxSeconds` are the tax a buy in the launch
  second actually pays, and how long it decays over. A sniper that ignores them
  is bidding against a number it has not read.
