//! Demand forecasting from consumption history.
//!
//! Pure maths, no database, so every judgement here is testable and visible.
//!
//! The question an operator is really asking is not "how much is left" but
//! "will it outlast the time it takes to replace it". That comparison —
//! **days of cover against lead time** — is the one number that turns a stock
//! level into a decision, and it is the thing a reorder threshold cannot
//! express: a fixed threshold is right for exactly one burn rate, and wrong
//! the moment consumption changes.
//!
//! Two kinds of honesty are built in rather than bolted on:
//!
//!   · A burn rate from two data points is arithmetic, not a forecast. Below a
//!     minimum sample the rate is withheld rather than published with false
//!     precision.
//!   · A mean hides its own spread. Consumption of 10,10,10 and of 0,0,30
//!     give the same average and completely different risk, so the variability
//!     travels with the estimate and the stockout projection is reported as a
//!     range, not a date.

use chrono::{DateTime, Duration, Utc};

/// Below this many separate consumption events, a rate is noise.
pub const MIN_EVENTS_FOR_RATE: usize = 3;

/// And below this many days of history, even several events can be a burst
/// rather than a trend.
pub const MIN_DAYS_OF_HISTORY: i64 = 7;

/// One stock withdrawal.
#[derive(Debug, Clone, Copy)]
pub struct Consumption {
    pub at: DateTime<Utc>,
    /// Positive magnitude of what left.
    pub amount: f64,
}

/// What the history supports saying.
#[derive(Debug, PartialEq)]
pub enum Confidence {
    /// Enough events over enough time, and reasonably steady.
    Good,
    /// Enough to estimate, but consumption is erratic — treat the projection
    /// as a range and not a date.
    Erratic,
    /// Not enough history to say anything. The rate is withheld.
    Insufficient,
}

impl Confidence {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Good => "good",
            Self::Erratic => "erratic",
            Self::Insufficient => "insufficient",
        }
    }
}

#[derive(Debug)]
pub struct Forecast {
    pub events: usize,
    pub window_days: f64,
    pub total_consumed: f64,
    /// None when the history cannot support one.
    pub burn_per_day: Option<f64>,
    /// Coefficient of variation of the per-event amounts: spread relative to
    /// the mean. Unitless, so seed in kg and water in litres are comparable.
    pub variability: Option<f64>,
    pub confidence: Confidence,
    /// How long the stock on hand lasts at the burn rate.
    pub days_of_cover: Option<f64>,
    /// Earliest and latest plausible stockout, widened by the variability.
    pub stockout_earliest: Option<DateTime<Utc>>,
    pub stockout_latest: Option<DateTime<Utc>>,
    /// True when the stock will not outlast resupply — already too late to
    /// order on time. The alarm worth raising.
    pub past_reorder_point: bool,
    /// The last date an order can be placed and still arrive before stockout.
    pub order_by: Option<DateTime<Utc>>,
    /// Enough to cover lead time plus a safety margin, net of what is on hand.
    pub recommended_quantity: Option<f64>,
}

/// Build a forecast for one item.
///
/// `now` is passed in rather than read from the clock so the result is
/// reproducible and testable.
pub fn forecast(
    consumption: &[Consumption],
    on_hand: f64,
    lead_time_days: Option<f64>,
    now: DateTime<Utc>,
) -> Forecast {
    let events = consumption.len();
    let total: f64 = consumption.iter().map(|c| c.amount).sum();

    // Window spans the observed history, not an arbitrary lookback: a single
    // event in a 90-day window would otherwise read as a very low rate when it
    // is really no information at all.
    let (oldest, newest) = consumption.iter().fold((None, None), |(o, n), c| {
        (
            Some(o.map_or(c.at, |x: DateTime<Utc>| x.min(c.at))),
            Some(n.map_or(c.at, |x: DateTime<Utc>| x.max(c.at))),
        )
    });
    let window_days = match (oldest, newest) {
        (Some(o), Some(n)) => ((n - o).num_seconds() as f64 / 86_400.0).max(0.0),
        _ => 0.0,
    };

    let insufficient = events < MIN_EVENTS_FOR_RATE || window_days < MIN_DAYS_OF_HISTORY as f64;

    if insufficient || total <= 0.0 || window_days <= 0.0 {
        return Forecast {
            events,
            window_days,
            total_consumed: total,
            burn_per_day: None,
            variability: None,
            confidence: Confidence::Insufficient,
            days_of_cover: None,
            stockout_earliest: None,
            stockout_latest: None,
            // Without a rate there is no basis to claim anything is urgent.
            // Saying "fine" would be as wrong as saying "critical".
            past_reorder_point: false,
            order_by: None,
            recommended_quantity: None,
        };
    }

    let burn = total / window_days;

    // Coefficient of variation over the per-event amounts.
    let mean = total / events as f64;
    let variance = consumption
        .iter()
        .map(|c| (c.amount - mean).powi(2))
        .sum::<f64>()
        / events as f64;
    let cv = if mean > 0.0 { variance.sqrt() / mean } else { 0.0 };

    // Above this, the mean is not describing the behaviour well enough to date
    // a stockout from it.
    let confidence = if cv > 0.75 { Confidence::Erratic } else { Confidence::Good };

    let cover = on_hand / burn;

    // Widen the projection by the observed variability, floored so a steady
    // item still gets a band rather than a falsely precise day.
    let spread = (cover * cv).max(cover * 0.1);
    let earliest = now + Duration::seconds(((cover - spread).max(0.0) * 86_400.0) as i64);
    let latest = now + Duration::seconds(((cover + spread) * 86_400.0) as i64);

    let lead = lead_time_days.unwrap_or(0.0);
    // The comparison that matters: does what is on the shelf outlast the time
    // it takes to replace it?
    let past = cover <= lead;
    let order_by = (!past)
        .then(|| now + Duration::seconds(((cover - lead).max(0.0) * 86_400.0) as i64));

    // Cover the lead time plus half again as margin, less what is already
    // here. Never negative.
    let recommended = ((burn * lead * 1.5) - on_hand).max(0.0);

    Forecast {
        events,
        window_days,
        total_consumed: total,
        burn_per_day: Some(burn),
        variability: Some(cv),
        confidence,
        days_of_cover: Some(cover),
        stockout_earliest: Some(earliest),
        stockout_latest: Some(latest),
        past_reorder_point: past,
        order_by,
        recommended_quantity: if lead > 0.0 { Some(recommended) } else { None },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(days_ago: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000, 0).unwrap() - Duration::days(days_ago)
    }
    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000, 0).unwrap()
    }

    /// Steady 10 units every 7 days over 4 weeks.
    fn steady() -> Vec<Consumption> {
        vec![
            Consumption { at: at(28), amount: 10.0 },
            Consumption { at: at(21), amount: 10.0 },
            Consumption { at: at(14), amount: 10.0 },
            Consumption { at: at(7), amount: 10.0 },
            Consumption { at: at(0), amount: 10.0 },
        ]
    }

    #[test]
    fn a_steady_burn_gives_a_rate_and_cover() {
        // 50 units over 28 days = 1.785…/day; 100 on hand ≈ 56 days of cover.
        let f = forecast(&steady(), 100.0, None, now());
        assert_eq!(f.confidence, Confidence::Good);
        let burn = f.burn_per_day.unwrap();
        assert!((burn - 50.0 / 28.0).abs() < 1e-9, "burn was {burn}");
        assert!((f.days_of_cover.unwrap() - 56.0).abs() < 0.1);
    }

    #[test]
    fn too_few_events_withholds_the_rate_entirely() {
        // Two withdrawals is arithmetic, not a forecast. Publishing a rate
        // from it would be false precision dressed as insight.
        let thin = vec![
            Consumption { at: at(10), amount: 5.0 },
            Consumption { at: at(0), amount: 5.0 },
        ];
        let f = forecast(&thin, 100.0, Some(30.0), now());
        assert_eq!(f.confidence, Confidence::Insufficient);
        assert!(f.burn_per_day.is_none());
        assert!(f.days_of_cover.is_none());
        // And it must not claim urgency it cannot justify.
        assert!(!f.past_reorder_point);
        assert!(f.recommended_quantity.is_none());
    }

    #[test]
    fn a_burst_inside_a_single_day_is_not_a_trend() {
        // Five withdrawals, all this morning. Enough events, no history.
        let burst: Vec<_> = (0..5)
            .map(|_| Consumption { at: at(0), amount: 4.0 })
            .collect();
        let f = forecast(&burst, 100.0, None, now());
        assert_eq!(f.confidence, Confidence::Insufficient);
        assert!(f.burn_per_day.is_none());
    }

    #[test]
    fn erratic_consumption_is_flagged_even_with_the_same_mean() {
        // 0,0,30 and 10,10,10 average identically and carry completely
        // different risk. The average alone cannot tell them apart.
        let lumpy = vec![
            Consumption { at: at(28), amount: 1.0 },
            Consumption { at: at(14), amount: 1.0 },
            Consumption { at: at(0), amount: 48.0 },
        ];
        let f = forecast(&lumpy, 100.0, None, now());
        assert_eq!(f.confidence, Confidence::Erratic);
        assert!(f.variability.unwrap() > 0.75);

        let smooth = forecast(&steady(), 100.0, None, now());
        assert_eq!(smooth.confidence, Confidence::Good);
        assert!(smooth.variability.unwrap() < 0.01);
    }

    #[test]
    fn the_stockout_is_a_range_not_a_date() {
        let f = forecast(&steady(), 100.0, None, now());
        let (e, l) = (f.stockout_earliest.unwrap(), f.stockout_latest.unwrap());
        assert!(e < l, "a projection with no width would be a false promise");
        // Even perfectly steady history gets a band.
        assert!((l - e).num_days() >= 10);
    }

    #[test]
    fn stock_that_cannot_outlast_resupply_is_the_alarm() {
        // 56 days of cover against a 90-day resupply: ordering today is
        // already too late, and no reorder threshold expresses that.
        let f = forecast(&steady(), 100.0, Some(90.0), now());
        assert!(f.past_reorder_point);
        assert!(f.order_by.is_none(), "there is no date that still works");

        // The same stock against a 10-day lead is comfortable.
        let ok = forecast(&steady(), 100.0, Some(10.0), now());
        assert!(!ok.past_reorder_point);
        assert!(ok.order_by.unwrap() > now());
    }

    #[test]
    fn order_by_leaves_exactly_the_lead_time_before_stockout() {
        let f = forecast(&steady(), 100.0, Some(10.0), now());
        let cover = f.days_of_cover.unwrap();
        let expected = now() + Duration::seconds(((cover - 10.0) * 86_400.0) as i64);
        assert!((f.order_by.unwrap() - expected).num_seconds().abs() <= 1);
    }

    #[test]
    fn a_well_stocked_item_is_recommended_nothing() {
        // Already holding more than the lead time needs.
        let f = forecast(&steady(), 10_000.0, Some(10.0), now());
        assert_eq!(f.recommended_quantity.unwrap(), 0.0);
    }

    #[test]
    fn no_history_at_all_is_handled() {
        let f = forecast(&[], 100.0, Some(30.0), now());
        assert_eq!(f.confidence, Confidence::Insufficient);
        assert_eq!(f.events, 0);
        assert!(f.burn_per_day.is_none());
    }
}
