# Strategy G's live twin (`G_live`): real money

`G_live` is strategy G's rule on a live book of its own. It places its
post-only NO bids on Polymarket with real money, beside paper G, which goes on
at its own size as the twin to compare against. Every other strategy stays on
paper.

> **Status of this build.** The whole live path is built and tested against a
> mock venue: the signer (bit-exact with the reference client's golden
> vectors), the API credentials, the post-only orders, their follow-up and
> cancels, the readiness checks, the live book, its limits and its loss stop.
> **The risk engine still refuses every live-mode intent** (`CheckId::Mode`,
> the Phase 14 gate). With `WM_LIVE_G=true` the venue connects and runs its
> checks, the dashboard's LIVE panel shows them, and G_live's proposals end
> in that refusal: no real order goes out until the gate is lifted for the
> live book in a reviewed change.

## 1. What it does

| | |
|---|---|
| Rule | G's: a NO bid one tick inside the book on each bucket at least `min_distance` degrees above the day's high whose YES is offered at 1–8¢, 10:00–21:00 local |
| Order | post-only, good till ten minutes before the next routine report (stated a minute later: the venue ends GTD orders a minute early); cancelled there too |
| Size | `[live].notional` ($10): about ten shares, sized down to whole cents |
| Limits | resting bids and positions together at most `max_open_usd` ($40); a UTC day's loss of `max_daily_loss_usd` ($20) stops it for the day |
| Stop | once its realized loss since going live reaches `max_total_loss_usd` ($20), earlier runs' settled days included, it opens nothing more |
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
7. the venue's positions in the open weather markets equal the live book's
   (checked at start, and every two minutes while they differ);
8. no order of unknown fate (an answer that proves nothing waits for the
   order's record; one without a record ten seconds after it was sent was
   never placed);
9. the collateral read, and enough of it beside what the resting bids reserve.

When any of them stops holding, the resting bids are pulled. The kill switch
pulls them too.

## 3. Switching it on

In the Portainer stack's environment (never in git):

| Variable | |
|---|---|
| `WM_LIVE_G` | `true` |
| `WM_POLY_PRIVATE_KEY` | the signer's private key (`0x…`). Read once at start, kept only in the signer, never logged |
| `WM_POLY_FUNDER` | the wallet that holds the money (for an e-mail or Magic login: the proxy wallet, as dailyprofit's `POLY_FUNDER`) |
| `WM_POLY_SIG_TYPE` | `1` for a proxy wallet (as dailyprofit's `POLY_SIG_TYPE`), `0` for an EOA, `2` for a Gnosis safe |

Then, on the dashboard: the LIVE panel shows the location the venue sees, the
protocol, the collateral, the heartbeat and every check that does not pass.

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
* The stop rule of the operator's plan: two losses in the first thirty live
  fills, or live G filled clearly less often than paper G, means stop and
  review (`WM_LIVE_G=false`, restart).
