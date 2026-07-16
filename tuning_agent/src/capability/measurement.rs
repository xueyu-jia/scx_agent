use serde_json::Value;

use crate::domain::{
    CapabilityMeta, CleanupReceipt, MeasurementOpenRequest, MeasurementSampleRequest,
    MeasurementSession, MetricBatch, ProviderError,
};

pub trait MeasurementProvider: Send + Sync {
    fn meta(&self) -> &CapabilityMeta;

    fn validate_specification(&self, specification: &Value) -> Result<(), ProviderError>;

    fn open(&self, request: &MeasurementOpenRequest) -> Result<MeasurementSession, ProviderError>;

    fn sample(&self, request: &MeasurementSampleRequest) -> Result<MetricBatch, ProviderError>;

    fn close(&self, session: &MeasurementSession) -> Result<CleanupReceipt, ProviderError>;
}
