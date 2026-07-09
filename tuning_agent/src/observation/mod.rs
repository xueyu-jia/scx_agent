mod snapshot;

pub use snapshot::CoreSnapshot;

#[derive(Default)]
pub struct Observation;

impl Observation {
    pub fn core_snapshot(&self) -> std::io::Result<CoreSnapshot> {
        CoreSnapshot::collect()
    }
}
