use serde::{Deserialize, Serialize};

use crate::domain::{EpisodeId, OperationId};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InvocationContext {
    pub episode_id: EpisodeId,
    pub operation_id: OperationId,
}
