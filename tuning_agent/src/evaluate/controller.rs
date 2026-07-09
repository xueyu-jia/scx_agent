use serde::Serialize;

use crate::act::{ActKernel, ApplyReport, RestoreReport};
use crate::evaluate::{EvaluationDecision, EvaluationKernel, EvaluationPlan};
use crate::observation::Observation;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum EvaluationPhase {
    Started,
    RestoringBaselinePrime,
    SamplingBaselinePrime,
    ApplyingCommitCandidate,
    SamplingCandidatePrime,
    Evaluating,
    Accepted,
    Rejected,
    Inconclusive,
    Frozen,
}

#[derive(Clone, Debug, Serialize)]
pub struct EvaluationReport {
    pub phase: EvaluationPhase,
    pub reason: String,
    pub baseline_restore: Option<RestoreReport>,
    pub candidate_apply: Option<ApplyReport>,
    pub decision: Option<EvaluationDecision>,
}

#[derive(Clone, Debug, Serialize)]
pub enum EvaluationOutcome {
    Accepted(EvaluationReport),
    Rejected(EvaluationReport),
    Inconclusive(EvaluationReport),
    Frozen(EvaluationReport),
}

pub struct EvaluationController {
    kernel: EvaluationKernel,
    phase: EvaluationPhase,
}

impl EvaluationController {
    pub fn new(kernel: EvaluationKernel) -> Self {
        Self {
            kernel,
            phase: EvaluationPhase::Started,
        }
    }

    pub fn validate_commit(
        &mut self,
        plan: &EvaluationPlan,
        act: &mut ActKernel,
        observation: &Observation,
    ) -> EvaluationOutcome {
        self.phase = EvaluationPhase::RestoringBaselinePrime;
        let baseline_restore = match act.restore_to_baseline() {
            Ok(report) => report,
            Err(err) => return self.frozen(format!("baseline restore failed: {err}"), None, None),
        };

        self.kernel.settle(plan);
        self.phase = EvaluationPhase::SamplingBaselinePrime;
        let baseline_prime = match self.kernel.sample(observation, act, plan) {
            Ok(sample) => sample,
            Err(err) => {
                return self.frozen(
                    format!("failed to sample restored baseline: {err}"),
                    Some(baseline_restore),
                    None,
                );
            }
        };

        self.phase = EvaluationPhase::ApplyingCommitCandidate;
        let candidate_apply = match act.apply_commit_candidate(&plan.keep_writes) {
            Ok(report) => report,
            Err(err) => {
                return self.frozen(
                    format!("failed to apply commit candidate: {err}"),
                    Some(baseline_restore),
                    None,
                );
            }
        };

        self.kernel.settle(plan);
        self.phase = EvaluationPhase::SamplingCandidatePrime;
        let candidate_prime = match self.kernel.sample(observation, act, plan) {
            Ok(sample) => sample,
            Err(err) => {
                return self.frozen(
                    format!("failed to sample commit candidate: {err}"),
                    Some(baseline_restore),
                    Some(candidate_apply),
                );
            }
        };

        self.phase = EvaluationPhase::Evaluating;
        let decision = self.kernel.evaluate(plan, baseline_prime, candidate_prime);

        match decision.verdict {
            crate::evaluate::EvaluationVerdict::Improved => {
                self.phase = EvaluationPhase::Accepted;
                EvaluationOutcome::Accepted(EvaluationReport {
                    phase: self.phase,
                    reason: plan.reason.clone(),
                    baseline_restore: Some(baseline_restore),
                    candidate_apply: Some(candidate_apply),
                    decision: Some(decision),
                })
            }
            crate::evaluate::EvaluationVerdict::Inconclusive => {
                self.phase = EvaluationPhase::Inconclusive;
                EvaluationOutcome::Inconclusive(EvaluationReport {
                    phase: self.phase,
                    reason: "evaluation was inconclusive; workload was not comparable".to_string(),
                    baseline_restore: Some(baseline_restore),
                    candidate_apply: Some(candidate_apply),
                    decision: Some(decision),
                })
            }
            _ => {
                self.phase = EvaluationPhase::Rejected;
                EvaluationOutcome::Rejected(EvaluationReport {
                    phase: self.phase,
                    reason: "evaluation rejected commit".to_string(),
                    baseline_restore: Some(baseline_restore),
                    candidate_apply: Some(candidate_apply),
                    decision: Some(decision),
                })
            }
        }
    }

    fn frozen(
        &mut self,
        reason: String,
        baseline_restore: Option<RestoreReport>,
        candidate_apply: Option<ApplyReport>,
    ) -> EvaluationOutcome {
        self.phase = EvaluationPhase::Frozen;
        EvaluationOutcome::Frozen(EvaluationReport {
            phase: self.phase,
            reason,
            baseline_restore,
            candidate_apply,
            decision: None,
        })
    }
}
