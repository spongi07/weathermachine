//! The strategies as the dashboard shows them: what each one does, whether
//! it runs, its settings (straight from the configuration file, so they
//! cannot drift) and, for strategy F, its time slots.

use crate::config::AppConfig;
use chrono::{DateTime, Datelike, Utc};
use chrono_tz::Tz;
use serde::Serialize;
use wm_core::time::{Season, local_minute_of_day};
use wm_dashboard_api::{PeakSlotDto, SeasonSlotDto, SlotTodayDto, StrategyDto};
use wm_strategy::{PeakSlotConfig, PeakTimes};

/// Engine id of strategy F.
pub const PEAK_SLOT_ID: &str = "F_peak_slot";

/// Seasons in calendar order.
const SEASONS: [Season; 4] = [
    Season::Winter,
    Season::Spring,
    Season::Summer,
    Season::Autumn,
];

/// Engine id of the unwind engine (always listed: its exits are orders too).
pub const UNWIND_ID: &str = "U_unwind";

/// The strategies the dashboard shows: those switched on, in the order the
/// engine evaluates them, plus the unwind engine, then the strategy lab's
/// paper strategies. A strategy switched off in the configuration is not
/// shown: A–E were taken out of the live bot on 1 October 2026 after two
/// days without a trade (E already earlier). Their code stays, because
/// `research market` replays A, B and E as baselines, and switching one on
/// again brings it back here.
pub fn catalog(cfg: &AppConfig) -> Vec<StrategyDto> {
    every_strategy(cfg)
        .into_iter()
        .filter(|s| s.enabled || s.id == UNWIND_ID)
        .collect()
}

/// Every strategy of the engine, switched on or off.
pub fn every_strategy(cfg: &AppConfig) -> Vec<StrategyDto> {
    let st = &cfg.file.strategies;
    // The unwind engine's summary names the strategies it leaves alone, as
    // configured, so it cannot drift from `exempt_strategies`.
    let exempt: Vec<&str> = st
        .unwind
        .exempt_strategies
        .iter()
        .map(|s| s.split('_').next().unwrap_or(s))
        .collect();
    let left_alone = if exempt.is_empty() {
        "the lab's positions".to_owned()
    } else {
        format!(
            "the positions of the strategies that hold to settlement ({}) and the lab's",
            exempt.join(", ")
        )
    };
    vec![
        entry(
            "A_buy_yes_final_high",
            "Buy YES on the final high",
            st.buy_yes.enabled,
            "Buys YES on the bucket holding the day's high once the model says the high is final with an edge over the ask, after the confirmation time. The model is pooled with the market's price: the book can veto a trade, never create one, and a book too wide to check the model blocks it.",
            &st.buy_yes,
        ),
        entry(
            "B_buy_no_above_high",
            "Buy NO above the high",
            st.buy_no.enabled,
            "Buys NO on buckets above the day's high when the model gives the temperature little chance to climb that far, with an edge over the NO ask. Pooled with the market like A: no trade on a book too wide to check the model.",
            &st.buy_no,
        ),
        entry(
            "C_split_unwind",
            "Split + unwind (research)",
            st.split_unwind.enabled,
            "Research only: splits a position into YES and NO legs and unwinds the loser. The risk engine rejects it outside backtests.",
            &st.split_unwind,
        ),
        entry(
            "D_certain_outcome",
            "Decided outcomes",
            st.certain.enabled,
            "Buys outcomes the observations have already decided (buckets below a new high are dead, so their NO wins), on the report itself, while stale quotes are still offered.",
            &st.certain,
        ),
        entry(
            "E_book_confirmed_high",
            "Book-confirmed high",
            st.book_confirmed.enabled,
            "Buys YES on the high's bucket at 0.90–0.99 between two local times, once the high is old enough, the temperature has dropped and the order book is shrinking.",
            &st.book_confirmed,
        ),
        entry(
            PEAK_SLOT_ID,
            "Peak slot",
            st.peak_slot.enabled,
            "Buys YES on the bucket holding the day's high inside the season's peak slot (when the station's history usually first reports its high), once that bucket is offered above the minimum price: a fixed number of shares, all at or below the maximum price, one position a day.",
            &st.peak_slot,
        ),
        entry(
            wm_strategy::tail_seller::ID,
            "Tail seller",
            st.tail_seller.enabled,
            "Sells the far tails: rests a NO bid, one tick inside the spread, on every bucket two or more degrees above the day's high whose YES is offered cheaply — an offer of that longshot to the takers who buy it — when the model rates the bucket no higher than that price. Orders expire ten minutes before each report; a filled order is held to settlement.",
            &st.tail_seller,
        ),
        entry(
            wm_strategy::next_degree::ID,
            "Next degree",
            st.next_degree.enabled,
            "Buys YES on the bucket one degree above the day's high while the day can still warm (until the season's late peak time, the temperature near the high) when it is offered cheaply and the model rates it at least at the ask. Held to settlement.",
            &st.next_degree,
        ),
        entry(
            wm_strategy::middle_fade::ID,
            "Middle fade",
            st.middle_fade.enabled,
            "Buys NO on buckets whose YES trades in the middle of the range (0.30–0.70) when the model rates them lower, if the NO is cheap enough after the measured overpricing of the middle, the fee and slippage. Held to settlement.",
            &st.middle_fade,
        ),
        entry(
            wm_strategy::morning_maker::ID,
            "Morning maker",
            st.morning_maker.enabled,
            "In the morning, rests a YES bid and a NO bid one tick inside the spread on every bucket priced in the middle; orders expire ten minutes before each report. When both sides fill the spread is earned whatever the weather; a side that fills alone is held to settlement.",
            &st.morning_maker,
        ),
        entry(
            wm_strategy::knmi_nowcast::ID,
            "KNMI nowcast",
            st.knmi_nowcast.enabled,
            "Reads KNMI's ten-minute observations at the airport. When the reading just before the next METAR is already well above the high's rounding edge, buys the NO of the high's bucket before the METAR reports the new high. Needs WM_KNMI_API_KEY; without it K stays idle.",
            &st.knmi_nowcast,
        ),
        entry(
            UNWIND_ID,
            "Unwind (exits)",
            st.unwind.enabled,
            &format!(
                "Sells positions the evidence has turned against (the win probability fell below the exit level, or the holding time ran out). It leaves alone {left_alone} (they trade their own books)."
            ),
            &st.unwind,
        ),
    ]
    .into_iter()
    .chain(lab_entries(cfg))
    .collect()
}

/// The strategy lab's 25 strategies, L1 first: each a paper strategy on its
/// own book (`lab` on the dashboard).
pub fn lab_entries(cfg: &AppConfig) -> Vec<StrategyDto> {
    let lab = &cfg.file.strategies.lab;
    // As written in the configuration file, like A–K's settings.
    let usd = |u: wm_core::units::Usd| u.to_string().trim_start_matches('$').to_owned();
    (1..=wm_strategy::lab::FAMILIES as u8)
        .map(|family| {
            let variant = lab.variant(family);
            let id = wm_strategy::lab::id_of(family).unwrap_or_default();
            let code = wm_strategy::lab::code(family);
            let mut settings = vec![
                (
                    "rule".to_owned(),
                    if variant {
                        format!("variant (listed in strategies.lab.variants as {code})")
                    } else {
                        "main rule".to_owned()
                    },
                ),
                (
                    "notional".to_owned(),
                    if family == 3 {
                        format!("F's {} shares", cfg.peak_slot().shares)
                    } else {
                        usd(lab.notional)
                    },
                ),
            ];
            let book = &lab.risk;
            let position = if family == 3 {
                book.f_escape_position_usd
            } else {
                book.position_size_usd
            };
            settings.extend([
                ("book.position_size_usd".to_owned(), usd(position)),
                ("book.max_exposure_usd".to_owned(), usd(book.max_exposure_usd)),
                (
                    "book.max_daily_new_exposure_usd".to_owned(),
                    usd(book.max_daily_new_exposure_usd),
                ),
                (
                    "book.max_daily_loss_usd".to_owned(),
                    usd(book.max_daily_loss_usd),
                ),
                ("book.max_spread".to_owned(), book.max_spread.to_string()),
                (
                    "max_reading_age_minutes".to_owned(),
                    lab.max_reading_age_minutes.to_string(),
                ),
                (
                    "slippage_allowance".to_owned(),
                    lab.slippage_allowance.to_string(),
                ),
            ]);
            StrategyDto {
                id: id.to_owned(),
                letter: code,
                name: wm_strategy::lab::NAMES[usize::from(family - 1)].to_owned(),
                enabled: lab.runs(family),
                summary: format!(
                    "{} Paper only, on its own book: it never blocks, nor is blocked by, A–K or another lab strategy.",
                    wm_strategy::lab::summary(family, variant)
                ),
                settings,
                peak_slot: None,
                lab: true,
            }
        })
        .collect()
}

fn entry<T: Serialize>(
    id: &str,
    name: &str,
    enabled: bool,
    summary: &str,
    section: &T,
) -> StrategyDto {
    let mut settings = Vec::new();
    if let Ok(v) = serde_json::to_value(section) {
        flatten("", &v, &mut settings);
    }
    settings.retain(|(k, _)| k != "enabled");
    StrategyDto {
        id: id.to_owned(),
        letter: id.split('_').next().unwrap_or(id).to_owned(),
        name: name.to_owned(),
        enabled,
        summary: summary.to_owned(),
        settings,
        peak_slot: None,
        lab: false,
    }
}

/// `{"a": {"b": 1}}` → `[("a.b", "1")]`; strings without quotes.
fn flatten(prefix: &str, v: &serde_json::Value, out: &mut Vec<(String, String)>) {
    use serde_json::Value;
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                let key = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                flatten(&key, x, out);
            }
        }
        Value::String(s) => out.push((prefix.to_owned(), s.clone())),
        Value::Null => out.push((prefix.to_owned(), "—".to_owned())),
        other => out.push((prefix.to_owned(), other.to_string())),
    }
}

/// `HH:MM` of a local minute.
fn hm(minute: u16) -> String {
    format!("{:02}:{:02}", minute / 60, minute % 60)
}

fn season_name(s: Season) -> &'static str {
    s.as_str()
}

/// A season from its name (as the views report it).
pub fn season_from_name(name: &str) -> Option<Season> {
    SEASONS.into_iter().find(|s| s.as_str() == name)
}

fn quantile_name(q: f64) -> String {
    if (q - 0.5).abs() < 1e-9 {
        "median".to_owned()
    } else {
        format!("{:.0}%", 100.0 * q)
    }
}

fn duration(minutes: u16) -> String {
    match (minutes / 60, minutes % 60) {
        (0, m) => format!("{m} min"),
        (h, 0) => format!("{h} h"),
        (h, m) => format!("{h} h {m} min"),
    }
}

/// Where a local minute stands against a slot `[start, end)`.
fn slot_status(minute: u16, (start, end): (u16, u16)) -> (bool, String) {
    if minute < start {
        (
            false,
            format!("before the slot: it starts in {}", duration(start - minute)),
        )
    } else if minute < end {
        (
            true,
            format!("inside the slot: it ends in {}", duration(end - minute)),
        )
    } else {
        (false, format!("after the slot: it ended at {}", hm(end)))
    }
}

/// A location for today's slot: its id, time zone and today's season (as
/// the engine's view reports it; else from the month).
pub struct SlotLocation<'a> {
    pub location: &'a str,
    pub tz: Tz,
    pub season: Option<Season>,
}

/// Strategy F's slots: per season from the installed model's peak times
/// (the fallback where it has none), and where each location stands today.
pub fn peak_slot(
    cfg: &PeakSlotConfig,
    peak_times: Option<&PeakTimes>,
    locations: &[SlotLocation<'_>],
    now: DateTime<Utc>,
) -> PeakSlotDto {
    let learned = peak_times.is_some_and(|p| !p.seasons.is_empty());
    let source = match peak_times.filter(|_| learned) {
        Some(p) => format!(
            "{} → {} of the local time the day's high was first reported, per season ({} days of METAR history{}); a season without history uses the fallback slot",
            quantile_name(cfg.slot_from_quantile),
            quantile_name(cfg.slot_to_quantile),
            p.seasons.iter().map(|s| s.days).sum::<u32>(),
            match (p.from, p.to) {
                (Some(f), Some(t)) => format!(", {f} → {t}"),
                _ => String::new(),
            }
        ),
        None => "fallback slots: the installed model carries no peak times yet (it learns them at its next training)".to_owned(),
    };
    let seasons = SEASONS
        .into_iter()
        .map(|season| {
            let ((start, end), _) = cfg.slot(season, peak_times);
            let history = peak_times.and_then(|p| p.season(season)).filter(|_| {
                peak_times
                    .and_then(|p| p.slot(season, cfg.slot_from_quantile, cfg.slot_to_quantile))
                    .is_some()
            });
            SeasonSlotDto {
                season: season_name(season).to_owned(),
                start: hm(start),
                end: hm(end),
                days: history.map_or(0, |h| h.days),
                mean: history.and_then(|h| h.mean()).map(|m| hm(m.round() as u16)),
                median: history.and_then(|h| h.quantile(0.5)).map(hm),
                q90: history.and_then(|h| h.quantile(0.9)).map(hm),
                later_than_slot: history.map(|h| h.share_after(end.saturating_sub(1))),
            }
        })
        .collect();
    let today = locations
        .iter()
        .map(|l| {
            let local = now.with_timezone(&l.tz);
            let season = l
                .season
                .unwrap_or_else(|| Season::from_month(local.month(), false));
            let ((start, end), _) = cfg.slot(season, peak_times);
            let minute = local_minute_of_day(now, l.tz);
            let (inside, status) = slot_status(minute, (start, end));
            SlotTodayDto {
                location: l.location.to_owned(),
                season: season_name(season).to_owned(),
                local_time: hm(minute),
                start: hm(start),
                end: hm(end),
                inside,
                status,
            }
        })
        .collect();
    PeakSlotDto {
        source,
        today,
        seasons,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use wm_strategy::{ObsPoint, PeakTimesBuilder};

    fn repo_root() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap()
    }

    fn shipped() -> AppConfig {
        AppConfig::load(Some(&repo_root().join("configs/weather-machine.toml"))).unwrap()
    }

    #[test]
    fn the_strategies_switched_on_are_listed_with_their_settings() {
        let cfg = shipped();
        let c = catalog(&cfg);
        let ids: Vec<&str> = c.iter().filter(|s| !s.lab).map(|s| s.id.as_str()).collect();
        assert_eq!(
            ids,
            ["F_peak_slot", "G_tail_seller", "K_knmi_nowcast", "U_unwind"]
        );
        let letters: String = c
            .iter()
            .filter(|s| !s.lab)
            .map(|s| s.letter.as_str())
            .collect();
        assert_eq!(letters, "FGKU");
        // The unwind engine names the strategies it leaves alone.
        let u = c.iter().find(|s| s.id == UNWIND_ID).unwrap();
        assert!(
            u.summary
                .contains("hold to settlement (F, G, H, I, J, K) and the lab's"),
            "{}",
            u.summary
        );
        // The lab's paper strategies follow, lowest number first, every one
        // running its main rule; the families switched off on 8 and 9
        // October 2026 are not shown.
        let off = [1, 9, 12, 17, 19, 21, 22, 23];
        let lab: Vec<&StrategyDto> = c.iter().filter(|s| s.lab).collect();
        assert_eq!(
            lab.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            wm_strategy::lab::IDS
                .iter()
                .enumerate()
                .filter(|(i, _)| !off.contains(&(i + 1)))
                .map(|(_, id)| *id)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            lab.iter().map(|s| s.letter.clone()).collect::<Vec<_>>(),
            (1..=25usize)
                .filter(|n| !off.contains(n))
                .map(|n| format!("L{n}"))
                .collect::<Vec<_>>()
        );
        assert!(c.iter().skip(4).all(|s| s.lab), "after the unwind engine");
        let setting = |s: &StrategyDto, k: &str| {
            s.settings
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.clone())
        };
        assert!(
            lab.iter()
                .all(|s| setting(s, "rule").as_deref() == Some("main rule"))
        );
        let family = |letter: &str| *lab.iter().find(|s| s.letter == letter).unwrap();
        assert_eq!(setting(family("L2"), "notional").as_deref(), Some("20.00"));
        assert_eq!(
            setting(family("L3"), "notional").as_deref(),
            Some("F's 100 shares")
        );
        assert_eq!(
            setting(family("L3"), "book.position_size_usd").as_deref(),
            Some("100.00")
        );
        assert!(lab.iter().all(|s| s.summary.contains("Paper only")));
        // A family switched off leaves the list; a variant says so.
        let mut fewer = cfg.clone();
        fewer.file.strategies.lab.disabled = vec!["L19".into(), "L20".into()];
        fewer.file.strategies.lab.variants = vec!["L2".into()];
        let c2 = catalog(&fewer);
        assert_eq!(c2.iter().filter(|s| s.lab).count(), 23);
        assert!(!c2.iter().any(|s| s.letter == "L19" || s.letter == "L20"));
        let l2 = c2.iter().find(|s| s.letter == "L2").unwrap();
        assert!(
            setting(l2, "rule").unwrap().starts_with("variant"),
            "{:?}",
            l2.settings
        );
        assert!(l2.summary.contains("0.6 °C"), "{}", l2.summary);
        // A–E and H–J are off and not shown; switched on again they come back.
        let all: String = every_strategy(&cfg)
            .iter()
            .filter(|s| !s.lab)
            .map(|s| s.letter.as_str())
            .collect();
        assert_eq!(all, "ABCDEFGHIJKU");
        assert_eq!(every_strategy(&fewer).iter().filter(|s| s.lab).count(), 25);
        let mut again = cfg.clone();
        again.file.strategies.buy_yes.enabled = true;
        assert_eq!(catalog(&again)[0].id, "A_buy_yes_final_high");
        let g = c.iter().find(|s| s.letter == "G").unwrap();
        let get_g = |k: &str| {
            g.settings
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get_g("max_yes_price"), Some("0.08"));
        assert_eq!(get_g("notional"), Some("30.00"));
        let f = c.iter().find(|s| s.id == PEAK_SLOT_ID).unwrap();
        assert!(f.enabled);
        let get = |k: &str| {
            f.settings
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("max_price"), Some("0.95"));
        assert_eq!(get("shares"), Some("100"));
        assert_eq!(get("fallback_slots.summer"), Some("15:00-18:00"));
        assert_eq!(get("enabled"), None, "shown as a pill, not a setting");
        assert!(c.iter().all(|s| s.enabled), "only strategies that run");
        assert!(
            every_strategy(&cfg)
                .iter()
                .all(|s| !s.summary.is_empty() && !s.settings.is_empty())
        );
    }

    fn summer_peaks() -> PeakTimes {
        let mut b = PeakTimesBuilder::new();
        let start: DateTime<Utc> = "2026-06-30T22:25:00Z".parse().unwrap();
        // Five days first at 14:25 (index 28), five at 16:25 (index 32).
        for idx in [28usize, 28, 28, 28, 28, 32, 32, 32, 32, 32] {
            let points: Vec<ObsPoint> = (0..48u16)
                .map(|i| {
                    let minute = (25 + 30 * i) % 1440;
                    ObsPoint {
                        observed_at: start + chrono::Duration::minutes(30 * i64::from(i)),
                        local_minute_of_day: minute,
                        local_minute_of_hour: (minute % 60) as u8,
                        temp: wm_core::units::TempC::from_whole(if usize::from(i) == idx {
                            25
                        } else {
                            15
                        }),
                        report_type: wm_core::weather::ReportType::Metar,
                        version: 1,
                    }
                })
                .collect();
            b.add_day(
                chrono::NaiveDate::from_ymd_opt(2026, 7, 1).unwrap(),
                Season::Summer,
                &points,
                25,
            );
        }
        b.build()
    }

    #[test]
    fn f_shows_the_learned_slot_the_fallback_and_where_today_stands() {
        let cfg = PeakSlotConfig::default();
        let pt = summer_peaks();
        let amsterdam = [SlotLocation {
            location: "amsterdam",
            tz: chrono_tz::Europe::Amsterdam,
            season: Some(Season::Summer),
        }];
        // 13:12 local (CEST): before the 14:25–16:26 slot.
        let now: DateTime<Utc> = "2026-07-02T11:12:00Z".parse().unwrap();
        let d = peak_slot(&cfg, Some(&pt), &amsterdam, now);
        assert!(
            d.source.starts_with("median → 90% of the local time"),
            "{}",
            d.source
        );
        assert!(
            d.source.contains("10 days of METAR history"),
            "{}",
            d.source
        );
        let summer = d.seasons.iter().find(|s| s.season == "summer").unwrap();
        assert_eq!(
            (summer.start.as_str(), summer.end.as_str(), summer.days),
            ("14:25", "16:26", 10)
        );
        assert_eq!(summer.median.as_deref(), Some("14:25"));
        assert_eq!(summer.q90.as_deref(), Some("16:25"));
        assert_eq!(summer.mean.as_deref(), Some("15:25"));
        assert_eq!(summer.later_than_slot, Some(0.0));
        // Winter has no history: the fallback, without statistics.
        let winter = d.seasons.iter().find(|s| s.season == "winter").unwrap();
        assert_eq!(
            (winter.start.as_str(), winter.end.as_str(), winter.days),
            ("13:00", "16:00", 0)
        );
        assert!(winter.median.is_none());
        let t = &d.today[0];
        assert_eq!((t.local_time.as_str(), t.inside), ("13:12", false));
        assert_eq!(t.status, "before the slot: it starts in 1 h 13 min");
        // Inside, and after.
        let inside = peak_slot(
            &cfg,
            Some(&pt),
            &amsterdam,
            "2026-07-02T12:30:00Z".parse().unwrap(),
        );
        assert!(inside.today[0].inside);
        assert_eq!(
            inside.today[0].status,
            "inside the slot: it ends in 1 h 56 min"
        );
        let after = peak_slot(
            &cfg,
            Some(&pt),
            &amsterdam,
            "2026-07-02T14:26:00Z".parse().unwrap(),
        );
        assert_eq!(after.today[0].status, "after the slot: it ended at 16:26");
        // Without peak times: every season on its fallback.
        let none = peak_slot(&cfg, None, &amsterdam, now);
        assert!(none.source.starts_with("fallback slots"));
        assert!(none.seasons.iter().all(|s| s.days == 0));
        assert_eq!(none.today[0].start, "15:00");
    }

    #[test]
    fn durations_and_season_names_read_naturally() {
        assert_eq!(duration(5), "5 min");
        assert_eq!(duration(120), "2 h");
        assert_eq!(duration(133), "2 h 13 min");
        assert_eq!(season_from_name("autumn"), Some(Season::Autumn));
        assert_eq!(season_from_name("monsoon"), None);
    }
}
