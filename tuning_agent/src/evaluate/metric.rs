use std::collections::BTreeMap;

use crate::observation::CoreSnapshot;
use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Debug, Serialize)]
pub struct EvaluationSample {
    pub values: BTreeMap<String, f64>,
    pub raw_measurements: Vec<Value>,
}

impl EvaluationSample {
    pub fn from_core_snapshot(snapshot: &CoreSnapshot) -> Self {
        let mut values = BTreeMap::new();
        values.insert(
            "loadavg.1m".to_string(),
            parse_loadavg_1m(&snapshot.loadavg),
        );
        values.insert(
            "psi.cpu.full.avg10".to_string(),
            parse_psi_avg10(&snapshot.pressure_cpu, "full"),
        );
        values.insert(
            "psi.io.full.avg10".to_string(),
            parse_psi_avg10(&snapshot.pressure_io, "full"),
        );
        values.insert(
            "psi.memory.full.avg10".to_string(),
            parse_psi_avg10(&snapshot.pressure_memory, "full"),
        );
        values.insert(
            "psi.cpu.some.avg10".to_string(),
            parse_psi_avg10(&snapshot.pressure_cpu, "some"),
        );
        values.insert(
            "psi.io.some.avg10".to_string(),
            parse_psi_avg10(&snapshot.pressure_io, "some"),
        );
        values.insert(
            "psi.memory.some.avg10".to_string(),
            parse_psi_avg10(&snapshot.pressure_memory, "some"),
        );
        Self {
            values,
            raw_measurements: Vec::new(),
        }
    }

    pub fn from_measurement_json(value: Value) -> Result<Self, String> {
        let object = value
            .as_object()
            .ok_or_else(|| "measurement output must be a JSON object".to_string())?;
        let mut values = BTreeMap::new();
        for (key, value) in object {
            if let Some(number) = value.as_f64() {
                values.insert(key.clone(), number);
            } else if let Some(boolean) = value.as_bool() {
                values.insert(key.clone(), if boolean { 1.0 } else { 0.0 });
            }
        }
        Ok(Self {
            values,
            raw_measurements: vec![value],
        })
    }

    pub fn merge(mut self, other: Self) -> Self {
        self.values.extend(other.values);
        self.raw_measurements.extend(other.raw_measurements);
        self
    }

    pub fn median(samples: Vec<Self>) -> Self {
        let mut grouped: BTreeMap<String, Vec<f64>> = BTreeMap::new();
        let mut raw_measurements = Vec::new();
        for sample in samples {
            for (metric, value) in sample.values {
                grouped.entry(metric).or_default().push(value);
            }
            raw_measurements.extend(sample.raw_measurements);
        }

        let values = grouped
            .into_iter()
            .map(|(metric, mut values)| {
                values.sort_by(f64::total_cmp);
                let mid = values.len() / 2;
                let median = if values.len() % 2 == 0 {
                    (values[mid - 1] + values[mid]) / 2.0
                } else {
                    values[mid]
                };
                (metric, median)
            })
            .collect();

        Self {
            values,
            raw_measurements,
        }
    }

    pub fn get(&self, metric: &str) -> Option<f64> {
        self.values.get(metric).copied()
    }
}

fn parse_loadavg_1m(value: &str) -> f64 {
    value
        .split_whitespace()
        .next()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(0.0)
}

fn parse_psi_avg10(content: &str, row: &str) -> f64 {
    content
        .lines()
        .find(|line| line.starts_with(row))
        .and_then(|line| {
            line.split_whitespace()
                .find_map(|part| part.strip_prefix("avg10="))
        })
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or(0.0)
}
