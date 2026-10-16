# Strategy G's live twin (`G_live`): real money

`G_live` is strategy G's rule on a live book of its own. It places its
post-only NO bids on Polymarket with real money, beside paper G, which goes on
at its own size as the twin to compare against. Every other strategy stays on
paper.

> **Status of this build.** With `WM_LIVE_G=true`, **G_live places real
> orders.** The whole live path is tested against a mock venue: the signer
> (bit-exact with the reference client's golden vectors), the API
> credentials, the post-only orders, their follow-up and cancels, the
> readiness checks, the live book, its limits and its loss stop. The risk
> engine approves a live-mode intent of the live book only while the live
> venue reports itself ready (every check in §2) and the loss stop has not
> tripped; its compliance check refuses a location the API may not trade
> from. No test has seen the real venue's answers to post-only orders, its
> heartbeats or the neg-risk signing domain: the first orders are that test,
> so watch them (§4).

## 1. What it does

| | |
|---|---|
| Rule | G's: a NO bid one tick inside the book on each bucket at least `min_distance` degrees above the day's high whose YES is offered at 1–8¢, 10:00–21:00 local |
| Order | post-only, good till ten minutes before the next routine report (stated a minute later: the venue ends GTD orders a minute early); cancelled there too |
| Size | `[live].notional` ($25; $10 until 16 October 2026): about 26 shares, sized down to whole cents |
| Limits | resting bids and positions together at most `max_open_usd` ($75: three bids); a UTC day's loss of `max_daily_loss_usd` ($50) stops it for the day |
| Stop | once its realized loss since going live reaches `max_total_loss_usd` ($50), earlier runs' settled days included, it opens nothing more |
| Settlement | held to settlement; a win is paid out when its shares are redeemed (claim them on polymarket.com, as for dailyprofit) |

Its fills are a maker's: at its own price, without a fee.

## 2. Before an order goes out

The live venue (`crates/wm-live`) places an order only while all of these
hold; the dashboard's LIVE panel lists the ones that do not:

1. the API credentials derived from the wallet key (L1 signature, key nonce
   `api_key_nonce`, 1: dailyprofit derives 0, so the two never share the
   credentials the venue's heartbeat switch cancels by);
2. the CLOB's protocol version is one the signer knows (2: the CTF Exchange
   V2 or its neg-risk twin per market; 3: the Exchange V3);
3. the account is not in closed-only mode;
4. the venue's geoblock check places this server where the **API** may open
   positions (the website's flag is not that: it is true from the
   Netherlands, where the API is allowed; lists as dailyprofit keeps them);
5. the venue takes the heartbeats: ten seconds after they stop it cancels
   every open order of these credentials, so a crashed process leaves nothing
   resting;
6. the orders an earlier run left on the weather markets are cancelled (any
   other order of the account is left alone);
7. the venue holds no shares in the open weather markets beyond the live
   book's: those would be exposure no book tracks, an order of another run
   or one by hand (checked at start, and every two minutes while it holds
   them). A venue list short of the books adds no exposure and does not
   block; an alert names what is short. On 13 October, after a restart at
   23:38 UTC, the list held neither live position, most likely because it
   was read as one page of 500, the most shares first, on an account of
   many markets; it is read page by page now;
8. no order of unknown fate (an answer that proves nothing waits for the
   order's record; one without a record ten seconds after it was sent was
   never placed);
9. the collateral read (every minute), and enough of it beside what the
   resting bids reserve for the order at hand.

Until they all hold, G_live's proposals wait in its evaluation with the
reason (the strategy page shows it), and the risk engine's mode check
refuses any live intent that gets that far. When any of them stops holding,
the resting bids are pulled. The kill switch pulls them too.

Two more holds keep new bids back without pulling the resting ones (the
LIVE panel lists them too):

* **Collateral short of one bid:** less than `[live].notional` free beside
  what the resting bids hold. On 13 October, the first live day, $11.31 was
  left at the venue after two fills: one $9.30 bid fitted, a second did not.
  A balance under one bid, resting bids aside, is said by an alert (claim
  the winnings, or deposit), and so is its end.
* **A pause after a refused order:** a live order the venue refuses (or the
  live venue's own checks do) pauses new bids for 5 minutes, doubling to an
  hour while the refusals go on, back to 5 once the venue takes an order.
  Without it the strategy would propose the refused bid again at the next
  book update, many times a second; an alert names the pause and its
  reason.

## 3. Switching it on

In the Portainer stack's environment (never in git):

| Variable | |
|---|---|
| `WM_LIVE_G` | `true` |
| `WM_POLY_PRIVATE_KEY` | the signer's private key (`0x…`). Read once at start, kept only in the signer, never logged |
| `WM_POLY_FUNDER` | the wallet that holds the money (for an e-mail or Magic login: the proxy wallet, as dailyprofit's `POLY_FUNDER`) |
| `WM_POLY_SIG_TYPE` | `1` for a proxy wallet (as dailyprofit's `POLY_SIG_TYPE`), `0` for an EOA, `2` for a Gnosis safe |
| `WM_ADMIN_TOKEN` | any long random string: it arms the dashboard's kill switch, which pulls the live bids too |

Then, on the dashboard: the LIVE panel shows the location the venue sees, the
protocol, the collateral, the heartbeat and every check that does not pass.

**How much to keep at the venue.** G_live holds at most `max_open_usd`
($75) in positions and resting bids at a time. A position's money comes back
only once its market is resolved and the winning shares are claimed on
polymarket.com, usually the day after, so yesterday's positions still hold
their cost while today's bids need theirs: about $150 lets it bid without
ever being short, and $90–100 is enough when the winnings are claimed every
morning (a day's two fills at $25 hold about $47). Less is safe, only
slower: a bid that does not fit waits (§2). More than $150 is of no use
without higher limits. (Until 16 October 2026 the bids were $10 under a
$40 cap and $20 loss limits; 7 fills, all won, +$3.89.)

**One account, one live bot.** If dailyprofit trades live on the same
account, its start-up check sees G_live's resting bids and holds itself
(`LEDGER_INVALID`) until a restart with none, and both draw on one balance.
Switch dailyprofit's live trading off while G_live trades.

## 4. Watching it

* The LIVE panel: trading or not and why, collateral, reserved, resting bids,
  positions, worst case against the limit, realized today and since going
  live, the stop.
* Alerts: every placed, refused and filled live order (`LIVE order placed`,
  `LIVE fill (real money)`), every pull, every change of a check.
* `weather-machine report paper`: G_live's fills and its settled result on a
  line of its own, apart from the paper books.
* Push messages on the phone (Pushover, as dailyprofit's), with
  `WM_PUSHOVER_TOKEN` (the API token of an application created at
  pushover.net) and `WM_PUSHOVER_USER` (the user key; dailyprofit's
  `PUSHOVER_USER` is the same one) in the stack's environment. Check them
  with `docker exec <container> /usr/local/bin/weather-machine notify-test`
  (in Portainer: the container's *Console*, command
  `/usr/local/bin/weather-machine notify-test`; the image has no shell): it
  sends one message now and says what Pushover answered. Then:

  ```
  G_live bought 4 NO 18°C at 0.94
  $3.76 on Amsterdam 12 Oct; its bid of 26 shares at 0.94 expired with the rest unfilled. It pays $4.00 if the high is not 18°C.
  Open now $13.16 of $75.00. Venue balance $36.84.
  ```

  when a live bid ends having bought shares (one message per bid, however
  many pieces it filled in), and

  ```
  G_live won +$0.24
  Amsterdam 12 Oct settled at 14°C (the observed high; the venue resolves the market itself).
  NO 18°C: 4 shares at 0.94 ($3.76), won: $4.00 back.
  Since going live +$0.24; it stops at a loss of $50.00. Venue balance $36.84; a win is added once it is claimed there.
  ```

  when its market settles (once the local day is over and two hours more),
  one message per strategy that held a position in it.
  `WM_PUSHOVER_STRATEGIES=GL,G` adds paper G's result, the twin to compare
  against; `all` sends every strategy's, the lab's included. A message is
  never sent twice: a restart that settles a market again finds its key in
  `system_events` (`kind = 'notify'`). The dashboard's pre-trade gates show
  a *Notifications* row (it never blocks a trade): on, for whom, how many
  sent; or off and why (unset, a setting that cannot be used, or a message
  Pushover refused: then they stay off until a restart, and an alert says
  so). A wrong setting never stops the service.
* The stop rule of the operator's plan: two losses in the first thirty live
  fills, or live G filled clearly less often than paper G, means stop and
  review (`WM_LIVE_G=false`, restart).
