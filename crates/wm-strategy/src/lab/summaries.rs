//! What L26–L65 do, in a sentence or two (main rule or variant). Each
//! trades once its signal has held for a minute; a taker buy's limit lies
//! one cent above the ask it saw, its expected profit counted there.

/// The summary of family `family` (26–65); empty for any other.
pub(crate) fn summary(family: u8, variant: bool) -> &'static str {
    match (family, variant) {
        (26, false) => {
            "Aggressive. 08:00–14:00: an airport 60–300 km upwind (within 35° of the METAR wind, ≥ 8 kt) ≥ 2.5 °C warmer than Schiphol and at least the high's rounding edge, its air due before 16:00 → YES of the next degree at 0.05–0.35."
        }
        (26, true) => "As L26 with the airport ≥ 3.5 °C warmer.",
        (27, false) => {
            "Aggressive. 09:00–13:00: KNMI's dew point down ≥ 1.5 °C in an hour while its temperature rose ≥ 1.0 °C (thermals mixing dry air down) → YES of the bucket above the favourite at 0.04–0.30."
        }
        (27, true) => "As L27 with the dew point down ≥ 2.5 °C.",
        (28, false) => {
            "Aggressive. 00:00–16:00: a live bucket priced ≥ 0.05 under both neighbours, each ≥ 0.12 (a day's maximum has one peak) → its YES at 0.03–0.30."
        }
        (28, true) => "As L28 with a dip ≥ 0.08 between neighbours ≥ 0.20.",
        (29, false) => {
            "Aggressive. 11:00–15:00: KNMI's wind ≤ 3.5 m/s and ≤ 0.6 × an hour before (≥ 4 m/s then), sunny in two of the last three readings, KNMI no more than 1.2 °C under the edge → YES of the next degree at 0.05–0.35."
        }
        (29, true) => "As L29 with the wind ≤ 2.5 m/s at ≤ half its earlier speed.",
        (30, false) => {
            "Aggressive. 08:00–11:00: the favourite holds yesterday's high while the forecast maximum lies ≥ 1.5 °C from it (the market anchors on yesterday) → YES of the forecast's bucket at 0.05–0.35."
        }
        (30, true) => "As L30 with the forecast ≥ 2.5 °C from yesterday's high.",
        (31, false) => {
            "Aggressive. 09:00–14:00: Schiphol under a ceiling ≤ 2500 ft within 2 °C of the high, an airport 60–200 km upwind clear in its last two reports, its air due within 3 hours and before 16:00 → YES of the next degree at 0.05–0.30."
        }
        (31, true) => "As L31 with the clear air due within 2 hours.",
        (32, false) => {
            "Aggressive. 10:00–13:30: KNMI's gusts ≥ 4 m/s above its mean wind (≥ 3 m/s) in the last three readings after ≤ 2 m/s an hour before (thermals reaching the wind above) → YES of the next degree at 0.04–0.30."
        }
        (32, true) => "As L32 with gusts ≥ 5 m/s above the mean.",
        (33, false) => {
            "Aggressive. 08:00–18:00: a best YES bid worth ≥ $250 and ≥ 4 × the largest of the same bucket 2–10 minutes before → its YES at ≤ the wall + 0.03 (0.05–0.45)."
        }
        (33, true) => "As L33 with a wall of ≥ $500 and ≥ 6 ×.",
        (34, false) => {
            "Aggressive. 10:00–17:00: the next degree's best ask unchanged in price and size for 55 minutes while KNMI rose ≥ 1.0 °C in the hour (no more than 0.8 °C under the edge) → that ask at ≤ 0.40."
        }
        (34, true) => "As L34 with KNMI up ≥ 1.5 °C.",
        (35, false) => {
            "Aggressive. 10:00–15:00 (until the median peak + 60 min): 3–15 minutes after a report raised the high, the next degree up ≤ 0.05 since while KNMI still rose ≥ 0.3 °C in 30 minutes → YES of the next degree at 0.05–0.35."
        }
        (35, true) => "As L35 with the next degree up ≤ 0.02.",
        (36, false) => {
            "Defensive. 08:00–14:00: an airport 60–300 km upwind saw a front pass in the last 3 hours (≥ 2 °C down within 70 minutes, the wind veering 40–180° or QNH up), due at Schiphol before the median peak → NO of the next degree at 0.60–0.95 (p = 0.92)."
        }
        (36, true) => "As L36 with a drop of ≥ 3 °C.",
        (37, false) => {
            "Defensive. 10:00–16:00: KNMI's pressure down ≥ 2 hPa in three hours, KNMI ≥ 0.8 °C under the edge → NO of the next degree at 0.55–0.92 (p = 0.90)."
        }
        (37, true) => "As L37 with the pressure down ≥ 3 hPa.",
        (38, false) => {
            "Defensive. 09:00–21:00 on a calm tape (≤ 1500 shares in 30 minutes, within 35 % two-sided, no trade ≥ 200 shares in 15): NO bids on the buckets two and three above the favourite (YES offered 0.02–0.12) until 10 minutes before the next report."
        }
        (38, true) => "As L38 on the buckets three and four above the favourite.",
        (39, false) => {
            "Defensive. From the season's 75 % peak time to 22:00, every bucket above the next degree ≤ 0.015: when YES of the high's bucket and of the next degree cost ≤ 0.99 a pair at their limits with fees, the same shares of both (one of them pays 1)."
        }
        (39, true) => "As L39 with the pair at ≤ 0.98.",
        (40, false) => {
            "Defensive. After the season's median peak time until 23:00: the bucket two above the high priced ≥ 0.02 above the next degree (out of order) → its NO at 0.85–0.99 (p = 0.985)."
        }
        (40, true) => "As L40 with ≥ 0.01 out of order.",
        (41, false) => {
            "Defensive. 08:00–14:00: ≥ 4 of yesterday's METARs with rain and the forecast ≥ 1 °C above yesterday's high (the wet ground holds it back) → NO of the bucket a degree above the forecast's at 0.70–0.97 (p = 0.94)."
        }
        (41, true) => "As L41 after ≥ 8 rainy reports.",
        (42, false) => {
            "Defensive. 10:00–16:00: KNMI's humidity ≥ 93 % all hour, its temperature up ≤ 0.3 °C in the hour and ≥ 0.6 °C under the edge → NO of the next degree at 0.60–0.95 (p = 0.93)."
        }
        (42, true) => "As L42 with the humidity ≥ 97 %.",
        (43, false) => {
            "Defensive. 12:00–17:00: ≥ 80 % of KNMI's wind directions since 09:00 from the sea (240–020°), the METAR wind ≥ 8 kt, KNMI ≥ 0.6 °C under the edge → NO of the next degree at 0.65–0.96 (p = 0.94)."
        }
        (43, true) => "As L43 with ≥ 95 % from the sea.",
        (44, false) => {
            "Defensive. A wallet without a record on the settled days buys ≥ 20 YES at 0.02–0.10 of a bucket ≥ 2 °C above the high: a NO bid one tick under their price for 30 minutes."
        }
        (44, true) => "As L44 on buys of ≥ 50 shares.",
        (45, false) => {
            "Defensive. 07:00–22:00: a bucket's NO at its limit, fee and slippage ≥ 0.02 under the sum of the other buckets' best YES bids (it pays when any of them wins) → that NO at 0.50–0.99."
        }
        (45, true) => "As L45 with a gap of ≥ 0.04.",
        (46, false) => {
            "Defensive. 21:00–23:50: KNMI ≥ 3 °C under the edge and not rising, the high's bucket offered at 0.95–0.985 (holders recycling capital) → its YES (p = 0.997)."
        }
        (46, true) => "As L46 from 22:00 with the ask ≤ 0.98.",
        (47, false) => {
            "Defensive. 17:00–23:00: ≥ 5 small YES buys (≤ 20 shares at 0.02–0.15) in 30 minutes on the next degree or the one above, KNMI ≥ 1 °C under the edge → a NO bid on that bucket until 10 minutes before the next report."
        }
        (47, true) => "As L47 after ≥ 8 small buys.",
        (48, false) => {
            "Defensive. 07:00–11:30: the ladder's own mean ≥ 2 °C above the high while the high's bucket (no warming at all) is still ≥ 0.05 → its NO at 0.80–0.95 (p = 0.95)."
        }
        (48, true) => "As L48 with the mean ≥ 3 °C above the high.",
        (49, false) => {
            "Balanced. 08:00–12:00: the forecast maximum within 0.3 °C of an x.5 edge while the market prices one of the two degrees ≤ 0.55 × the other → YES of the cheaper one at 0.10–0.45."
        }
        (49, true) => "As L49 within 0.2 °C and ≤ 0.45 ×.",
        (50, false) => {
            "Balanced. 07:00–21:00: a live bucket moved ≥ 0.12 against 8–15 minutes before with no report, no forecast and KNMI flat (≤ 0.3 °C) → against the move: YES after a fall, NO after a rise (0.15–0.80)."
        }
        (50, true) => "As L50 on moves of ≥ 0.18.",
        (51, false) => {
            "Balanced. 10:00–15:00: the ladder's mean down ≥ 0.6 °C in 30 minutes while KNMI rose ≥ 0.5 °C → YES a degree above the mean (0.08–0.40); the mean up while KNMI fell → NO of the mean's bucket (0.55–0.92)."
        }
        (51, true) => "As L51 with the mean ≥ 0.9 °C against KNMI ≥ 0.8 °C.",
        (52, false) => {
            "Balanced. In the 12 minutes before a report: the next degree up ≥ 0.05 in 10 minutes while KNMI's reading ahead of the METAR is ≥ 0.4 °C under the edge → its NO at 0.40–0.92."
        }
        (52, true) => "As L52 on a run-up of ≥ 0.08.",
        (53, false) => {
            "Balanced. 10:00–13:00: a sine from the night's lowest METAR through KNMI now to the season's median peak time projects the maximum; ≥ 0.8 °C off the ladder's mean → YES of its bucket at 0.10–0.45."
        }
        (53, true) => "As L53 with the projection ≥ 1.2 °C off.",
        (54, false) => {
            "Balanced. 06:00–22:00: the buckets' YES prices adding up to ≥ 1.08 → NO of the bucket (0.20–0.70) carrying most of the excess (≥ 0.03) at 0.30–0.80."
        }
        (54, true) => "As L54 from a sum of ≥ 1.12.",
        (55, false) => {
            "Balanced. 12:00–17:00 within an hour of the median peak: KNMI's last three means within 0.2 °C of the edge while one of the high's bucket and the next degree is ≤ 0.35 and the other ≥ 0.55 → YES of the cheap one at 0.10–0.35."
        }
        (55, true) => "As L55 with KNMI within 0.1 °C of the edge.",
        (56, false) => {
            "Balanced. 10:00–16:00: Schiphol and the KNMI neighbours ≥ 3 °C apart in the same ten minutes while the favourite is ≥ 0.55 → the favourite's NO at 0.25–0.45."
        }
        (56, true) => "As L56 with the stations ≥ 4 °C apart.",
        (57, false) => {
            "Balanced. 10:00–18:00: the next degree up ≥ 0.06 in 4–7 minutes while the high's bucket has not fallen and KNMI is no more than 0.2 °C under the edge → the high's bucket's NO at 0.10–0.75."
        }
        (57, true) => "As L57 with the next degree up ≥ 0.10.",
        (58, false) => {
            "Balanced. 07:00–20:00: a bucket of a multiple of 5 °C (≥ 0.15 of the ladder) priced ≥ 0.08 above the share a normal fitted to the other buckets gives it → its NO at 0.40–0.85."
        }
        (58, true) => "As L58 with ≥ 0.12 over the normal.",
        (59, false) => {
            "Balanced. One round trip a day: 09:00–18:00 away from reports, the favourite (0.20–0.80) with a spread ≤ 0.03 and ≥ 3 × as many shares bid as offered (≥ 100) → its YES, sold at the bid at +0.03, −0.05, after 45 minutes or 10 minutes before a report."
        }
        (59, true) => "As L59 with ≥ 5 × as many shares bid.",
        (60, false) => {
            "Balanced. 08:00–20:00, not in the 10 minutes before a report: the favourite or a neighbour (0.10–0.60) whose best ask holds ≤ 50 shares with the next ask ≥ 0.10 above → offer YES halfway into the gap (a NO bid) until 10 minutes before the next report."
        }
        (60, true) => "As L60 on gaps of ≥ 0.15.",
        (61, false) => {
            "Aggressive. 09:00–16:00: a SPECI (off the routine minutes) known 3–20 minutes ago opening the sky or ending the rain, ≤ 2 °C under the high, the next degree up ≤ 0.05 since → its YES at 0.05–0.35."
        }
        (61, true) => {
            "The worsening: a SPECI bringing rain, thunder or a ceiling under 1000 ft → NO of the next degree at 0.55–0.92 (p = 0.90)."
        }
        (62, false) => {
            "Defensive. 11:00–15:00: haze in the last two reports (visibility ≤ 7 km in dry air, or HZ/FU) and ≥ 1 °C under the high → NO of the next degree at 0.60–0.95 (p = 0.93)."
        }
        (62, true) => "As L62 with visibility ≤ 5 km.",
        (63, false) => {
            "Balanced. 08:00–20:00: ≥ 3 taker buys of one bucket's YES at one price in 30 minutes adding up to ≥ 3 × the size its ask showed, the ask still standing (a hidden seller) → its NO at 0.20–0.80."
        }
        (63, true) => "As L63 with ≥ 5 × the size shown.",
        (64, false) => {
            "Aggressive. 09:00–18:00 away from reports: the next degree's offered shares down ≥ 60 % against 4–7 minutes before (≥ 100 then) while its price moved ≤ +0.03 → its YES at 0.05–0.40."
        }
        (64, true) => "As L64 with ≥ 75 % withdrawn.",
        (65, false) => {
            "Balanced. 08:00–11:00: the high first reached before 02:00, KNMI ≥ 1 °C under it and the forecast's rest of the day ≤ the high + 0.4 °C → YES of the high's bucket at 0.10–0.60."
        }
        (65, true) => "As L65 with the forecast ≥ 0.5 °C under the high.",
        _ => "",
    }
}
