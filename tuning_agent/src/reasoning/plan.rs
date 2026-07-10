use crate::tools::ToolInvocation;

#[derive(Clone, Debug)]
pub enum Plan {
    ToolCalls(Vec<ToolInvocation>),
    DryRun(ActionPlan),
}

#[derive(Clone, Debug)]
pub struct ActionPlan {
    pub summary: String,
    pub expected_effect: String,
}

#[derive(Clone, Debug)]
pub struct ReasoningOutput {
    pub raw_json: String,
    pub plan: Plan,
}
