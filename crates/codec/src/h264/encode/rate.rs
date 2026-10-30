//! Rate control: one quantizer per frame predicted before any macroblock
//! is coded, then bounded per-row corrections inside each band, so a frame
//! takes one pass and never more than two.
//!
//! Quantizers here are "effective": `qp + 6 * suppression`, where
//! suppression (dropping coefficients below `1 << suppression`) only
//! starts once `qp` is 51. Bits are modelled as
//! `model * cost * 2^(-slope * (q - 24) / 6)`, where `cost` comes from the
//! analysis pass. Desktop content does not share one slope: text loses
//! about a fifth of its bits per six steps of QP, photographs half. IDRs
//! and scene cuts, where the last frame says nothing, measure both
//! numbers with a trial encode of a few rows at two quantizers; P frames
//! refit the model after every frame.

/// Highest effective quantizer: QP 51 with ten levels of suppression.
pub(super) const MAX_Q: i32 = 51 + 6 * 10;
/// Lowest QP the encoder uses.
pub(super) const MIN_Q: i32 = 18;

/// Splits an effective quantizer into `(qp, suppression)`.
pub(super) fn split(q: i32) -> (u8, u8) {
    let q = q.clamp(MIN_Q, MAX_Q);
    if q <= 51 {
        (q as u8, 0)
    } else {
        (51, ((q - 51) as f64 / 6.).ceil() as u8)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Model {
    /// Bits per unit cost at effective quantizer 24.
    scale: f64,
    /// Halvings of the bits per six steps of quantizer.
    slope: f64,
}

impl Model {
    fn bits(&self, cost: f64, q: f64) -> f64 {
        self.scale * cost * (-self.slope * (q - 24.) / 6.).exp2()
    }
    fn quantizer(&self, cost: f64, bits: f64) -> f64 {
        24. + 6. / self.slope * (self.scale * cost / bits.max(1.)).log2()
    }
}

/// Before anything has been measured: a guess between text and photographs.
const INITIAL: Model = Model {
    scale: 0.15,
    slope: 0.6,
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Plan {
    /// Effective quantizer of the first pass.
    pub q: i32,
    /// Bits the frame should take.
    pub target: f64,
    /// Of those, bits for what changed.
    pub content: f64,
    /// Above this the frame is coded once more.
    pub limit: f64,
    /// Bits left for refining unchanged macroblocks.
    pub refine: f64,
    /// Coded as a scene cut: the IDR budget and model without an IDR.
    pub cut: bool,
    /// The model to use and refit: 0 intra (IDRs and cuts), 1 inter.
    kind: usize,
}

impl Plan {
    /// An IDR or a cut: worth a trial encode.
    pub(super) fn intra(&self) -> bool {
        self.kind == 0
    }
}

pub(super) struct RateControl {
    /// Bits per frame at the configured rate.
    budget: f64,
    /// Bits sent beyond (positive) or under (negative) the budget so far,
    /// bounded so neither a burst nor an idle stretch dominates.
    fill: f64,
    models: [Option<Model>; 2],
    /// Effective quantizer of the last coded frame.
    last_q: Option<i32>,
    /// Cost of the last coded frame.
    last_cost: f64,
}

impl RateControl {
    pub(super) fn new(kbps: u32, fps: u32) -> Self {
        Self {
            budget: kbps.max(1) as f64 * 1000. / fps.max(1) as f64,
            fill: 0.,
            models: [None; 2],
            last_q: None,
            last_cost: 0.,
        }
    }

    pub(super) fn budget(&self) -> f64 {
        self.budget
    }

    /// A new rate keeps the debt or credit in frames, not bits.
    pub(super) fn set_rate(&mut self, kbps: u32, fps: u32) {
        let budget = kbps.max(1) as f64 * 1000. / fps.max(1) as f64;
        self.fill *= budget / self.budget;
        self.budget = budget;
    }

    fn model(&self, kind: usize) -> Model {
        self.models[kind]
            .or(self.models[1 - kind])
            .unwrap_or(INITIAL)
    }

    /// `cost` is the frame's analysis cost, `intra` what it would cost
    /// without inter prediction, `changed` the share of macroblocks that
    /// changed; `refinable` whether any unchanged macroblock could be
    /// coded better than it was.
    pub(super) fn plan(
        &self,
        idr: bool,
        cost: f64,
        intra: f64,
        changed: f64,
        refinable: bool,
    ) -> Plan {
        let b = self.budget;
        // Most of the picture changed, inter prediction hardly helps, and
        // it costs far more than the last frame: a new scene. It gets a
        // larger budget and the intra model, and no smoothing toward the
        // last quantizer.
        let cut = !idr
            && self.last_q.is_some()
            && changed > 0.5
            && cost > intra * 0.5
            && cost > self.last_cost * 3.;
        let kind = usize::from(!(idr || cut));
        // An IDR may take four frames' worth, as before, but no longer
        // takes as many passes as it needs to get there.
        let (target, limit) = if idr {
            ((4. * b - 0.5 * self.fill).clamp(b, 4. * b), 5. * b)
        } else if cut {
            ((2. * b - 0.5 * self.fill).clamp(b, 2. * b), 3. * b)
        } else {
            let target = (b - 0.5 * self.fill).clamp(0.5 * b, 1.5 * b);
            (target, 1.3 * target)
        };
        // P frames keep a share for refining what stopped changing, so a
        // small change does not take every bit at an ever finer quantizer.
        let reserve = if kind == 1 && refinable {
            0.3 * target
        } else {
            0.
        };
        let content = target - reserve;
        let model = self.model(kind);
        let mut q = model.quantizer(cost.max(1.), content);
        if let (Some(last), 1) = (self.last_q, kind) {
            q = q.clamp((last - 4) as f64, (last + 6) as f64);
        }
        let q = (q.round() as i32).clamp(MIN_Q, MAX_Q);
        Plan {
            q,
            target,
            content,
            limit,
            refine: (target - model.bits(cost, q as f64)).max(reserve),
            cut,
            kind,
        }
    }

    /// The quantizer of the first trial: the model's guess.
    pub(super) fn first_trial(plan: &Plan) -> i32 {
        plan.q.clamp(20, 51)
    }

    /// The quantizer of a second trial after the first estimated `bits`
    /// at `q`, or `None` when the first was close enough to the target.
    pub(super) fn second_trial(&self, plan: &Plan, q: i32, bits: f64) -> Option<i32> {
        let step = 6. / self.model(plan.kind).slope * (bits.max(8.) / plan.content).log2();
        let next = (q + (step.round() as i32).clamp(-12, 12)).clamp(MIN_Q, MAX_Q);
        (next.abs_diff(q) > 1).then_some(next)
    }

    /// Fits the model of `plan`'s kind to trial estimates of the frame's
    /// bits at one or two quantizers, and sets the plan's quantizer from it.
    pub(super) fn fit(&mut self, plan: &mut Plan, cost: f64, trials: &[(i32, f64)]) {
        let cost = cost.max(1.);
        let (qa, ba) = trials[0];
        let ba = ba.max(8.);
        let (slope, low, high) = match trials.get(1) {
            // Bits fall as the quantizer rises; anything flatter or steeper
            // than this is a sampling accident.
            Some(&(qb, bb)) => (
                ((ba / bb.max(8.)).log2() * 6. / (qb - qa) as f64).clamp(0.15, 2.),
                qa.min(qb) - 3,
                qa.max(qb) + 3,
            ),
            None => (self.model(plan.kind).slope, qa - 2, qa + 2),
        };
        let model = Model {
            scale: ba / (cost * (-slope * (qa as f64 - 24.) / 6.).exp2()),
            slope,
        };
        self.models[plan.kind] = Some(model);
        // Trust the fit only near the measured points.
        plan.q = (model.quantizer(cost, plan.content).round() as i32)
            .clamp(low, high)
            .clamp(MIN_Q, MAX_Q);
        plan.refine = 0.;
    }

    /// The model's slope for `plan`, for row control.
    pub(super) fn slope(&self, plan: &Plan) -> f64 {
        self.model(plan.kind).slope
    }

    /// The quantizer for a second pass after the first gave `bits` at `q`.
    pub(super) fn replan(&self, plan: &Plan, q: f64, bits: f64) -> i32 {
        // Aim below the target: one more pass is all there is.
        let step = 6. / self.slope(plan) * (bits / (plan.content * 0.85)).log2();
        ((q + step.max(1.)).ceil() as i32).clamp(MIN_Q, MAX_Q)
    }

    /// Records a frame coded to `plan`: `content` is its bits less those
    /// spent refining unchanged macroblocks, `q` its cost-weighted
    /// effective quantizer.
    pub(super) fn update(&mut self, plan: &Plan, cost: f64, bits: f64, content: f64, q: f64) {
        // A frame counts in proportion to its size: small updates are
        // mostly per-macroblock overhead and refinement, which the model
        // does not describe.
        let weight = (content / (0.25 * self.budget)).min(1.) * 0.5;
        if cost >= 1. && weight > 0. {
            let model = self.model(plan.kind);
            let measured = content * (model.slope * (q - 24.) / 6.).exp2() / cost;
            let scale = match self.models[plan.kind] {
                Some(m) => m.scale * (1. - weight) + measured * weight,
                None => measured,
            };
            self.models[plan.kind] = Some(Model { scale, ..model });
            if plan.cut {
                // What follows a new scene looks like it, not like the old one.
                self.models[1] = self.models[0];
            }
        }
        self.last_q = Some(q.round() as i32);
        self.last_cost = cost;
        self.record(bits);
    }

    /// Counts the bits of a frame coded without a plan (all skipped).
    pub(super) fn record(&mut self, bits: f64) {
        self.fill = (self.fill + bits - self.budget).clamp(-2. * self.budget, 8. * self.budget);
    }
}

/// Per-row corrections inside one band, from that band's share of the
/// frame target and its row costs. Deterministic: it depends only on the
/// band's own input and output, never on other threads.
#[derive(Clone, Debug, Default)]
pub(super) struct RowControl {
    /// Bits for the band; zero disables control.
    pub target: f64,
    /// `weights[r]`: the share of the band's cost in row `r`.
    pub weights: Vec<f64>,
    /// Effective quantizer the plan was made for.
    pub base: i32,
    /// Halvings of the bits per six steps of quantizer.
    pub slope: f64,
}

impl RowControl {
    pub(super) fn new(
        target: f64,
        costs: &[u32],
        mbs_per_row: usize,
        base: i32,
        slope: f64,
    ) -> Self {
        // A floor per macroblock keeps headers and skips in the plan.
        let floor = 32. * mbs_per_row as f64;
        let total: f64 = costs.iter().map(|&c| c as f64 + floor).sum();
        Self {
            target,
            weights: costs.iter().map(|&c| (c as f64 + floor) / total).collect(),
            base,
            slope,
        }
    }

    fn scale(&self, steps: i32) -> f64 {
        (self.slope * steps as f64 / 6.).exp2()
    }

    /// The quantizer for row `row`, given the bits spent on earlier rows,
    /// `planned`: what the plan expected those rows to cost at the
    /// quantizers used, and the current quantizer.
    pub(super) fn next(&self, row: usize, spent: f64, planned: f64, current: i32) -> i32 {
        if self.target <= 0. || row == 0 || row >= self.weights.len() {
            return current;
        }
        let left: f64 = self.weights[row..].iter().sum();
        if planned < self.target * 0.02 || left <= 0. {
            return current;
        }
        // How far the band runs from the plan, and what the rest would
        // cost at the current quantizer.
        let ratio = spent / planned;
        let rest = ratio * self.target * left * self.scale(self.base - current);
        let projected = spent + rest;
        let wanted = if projected > self.target * 1.15 {
            let room = self.target - spent;
            if room <= self.target * 0.02 {
                MAX_Q
            } else {
                let need = 6. / self.slope * (ratio * self.target * left / room).log2();
                (self.base as f64 + need).ceil() as i32
            }
        } else if projected < self.target * 0.6 {
            current - 1
        } else {
            current
        };
        wanted
            .clamp(current - 1, current + 6)
            .clamp(self.base - 2, MAX_Q)
            .max(MIN_Q)
    }

    /// What the plan expects row `row` to cost at quantizer `q`.
    pub(super) fn planned(&self, row: usize, q: i32) -> f64 {
        self.target * self.weights.get(row).copied().unwrap_or(0.) * self.scale(self.base - q)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_quantizers_split_into_qp_and_suppression() {
        assert_eq!(split(10), (18, 0));
        assert_eq!(split(30), (30, 0));
        assert_eq!(split(51), (51, 0));
        assert_eq!(split(52), (51, 1));
        assert_eq!(split(57), (51, 1));
        assert_eq!(split(58), (51, 2));
        assert_eq!(split(500), (51, 10));
    }

    #[test]
    fn a_new_rate_rescales_debt_and_credit_is_bounded() {
        let mut rc = RateControl::new(10_000, 50);
        let plan = rc.plan(true, 1000., 1000., 1., false);
        rc.update(&plan, 1000., 4. * rc.budget(), 4. * rc.budget(), 30.);
        let debt = rc.fill / rc.budget;
        assert!(debt > 2.);
        rc.set_rate(1000, 50);
        assert!((rc.fill / rc.budget - debt).abs() < 1e-9);
        for _ in 0..100 {
            rc.record(0.);
        }
        assert_eq!(rc.fill, -2. * rc.budget);
    }

    #[test]
    fn trials_fit_the_slope_and_hit_the_target() {
        let mut rc = RateControl::new(8000, 30);
        let b = rc.budget();
        let cost = 10_000_000.;
        let mut plan = rc.plan(true, cost, cost, 1., false);
        assert!(plan.intra());
        assert!(plan.target <= 4. * b);
        // A frame whose bits halve every twelve steps: slope 0.5.
        let truth = Model {
            scale: 0.1,
            slope: 0.5,
        };
        let qa = RateControl::first_trial(&plan);
        let qb = rc
            .second_trial(&plan, qa, truth.bits(cost, qa as f64))
            .unwrap();
        rc.fit(
            &mut plan,
            cost,
            &[qa, qb].map(|q| (q, truth.bits(cost, q as f64))),
        );
        let bits = truth.bits(cost, plan.q as f64);
        assert!(
            bits <= plan.target * 1.07 && bits > plan.target * 0.9,
            "{plan:?} {bits}"
        );
    }

    #[test]
    fn p_frames_follow_the_measured_model() {
        let mut rc = RateControl::new(8000, 30);
        let b = rc.budget();
        let plan = rc.plan(true, 5000., 5000., 1., false);
        rc.update(&plan, 5000., b, b, 30.);
        let plan = rc.plan(false, 5000., 50_000., 1., false);
        rc.update(&plan, 5000., b, b, 30.);
        let plan = rc.plan(false, 5000., 50_000., 1., false);
        assert!(!plan.cut);
        assert!((plan.q - 30).abs() <= 1, "{plan:?}");
        // Twice the cost at the same target is one halving of the bits,
        // ten steps at this slope; P frames move at most six at a time.
        let plan = rc.plan(false, 10000., 50_000., 1., false);
        assert_eq!(plan.q, 36, "{plan:?}");
        // Inter prediction no help and ten times the cost: a scene cut,
        // unless most of the picture is unchanged.
        let plan = rc.plan(false, 50_000., 50_000., 0.9, false);
        assert!(
            plan.cut && plan.intra() && plan.target > 1.5 * b,
            "{plan:?}"
        );
        assert!(!rc.plan(false, 50_000., 50_000., 0.3, false).cut);
        // A refinement-only frame leaves the model alone.
        let before = rc.models;
        let plan = rc.plan(false, 0., 50_000., 0., false);
        rc.update(&plan, 0., b, 0., 30.);
        assert_eq!(rc.models, before);
    }

    #[test]
    fn rows_over_plan_raise_the_quantizer_and_rows_under_lower_it() {
        let rows = RowControl::new(10_000., &[100; 10], 4, 30, 1.);
        // On plan: unchanged.
        let planned = rows.planned(0, 30) * 3.;
        assert_eq!(rows.next(3, planned, planned, 30), 30);
        // Twice the plan after three rows needs a coarser quantizer.
        let q = rows.next(3, planned * 2., planned, 30);
        assert!((34..=36).contains(&q), "{q}");
        // The whole band spent already: as coarse as one step allows.
        assert_eq!(rows.next(3, 10_000., planned, 30), 36);
        // Far under plan: one step finer, never below base - 2.
        assert_eq!(rows.next(3, planned * 0.2, planned, 30), 29);
        assert_eq!(rows.next(3, planned * 0.2, planned, 28), 28);
        // Control off.
        assert_eq!(RowControl::default().next(3, 1e9, 1., 30), 30);
    }
}
