use std::fs;

use crate::types::now_ns;

#[derive(Clone, Debug)]
pub struct CoreSnapshot {
    pub timestamp_ns: u128,
    pub loadavg: String,
    pub stat: String,
    pub meminfo: String,
    pub pressure_cpu: String,
    pub pressure_memory: String,
    pub pressure_io: String,
    pub net_snmp: String,
    pub net_softnet_stat: String,
}

impl CoreSnapshot {
    pub fn collect() -> std::io::Result<Self> {
        Ok(Self {
            timestamp_ns: now_ns(),
            loadavg: read_trimmed("/proc/loadavg")?,
            stat: read_file("/proc/stat")?,
            meminfo: read_file("/proc/meminfo")?,
            pressure_cpu: read_file_or_empty("/proc/pressure/cpu"),
            pressure_memory: read_file_or_empty("/proc/pressure/memory"),
            pressure_io: read_file_or_empty("/proc/pressure/io"),
            net_snmp: read_file_or_empty("/proc/net/snmp"),
            net_softnet_stat: read_file_or_empty("/proc/net/softnet_stat"),
        })
    }
}

fn read_file(path: &str) -> std::io::Result<String> {
    fs::read_to_string(path)
}

fn read_trimmed(path: &str) -> std::io::Result<String> {
    let mut value = read_file(path)?;
    while value.ends_with(['\n', '\r']) {
        value.pop();
    }
    Ok(value)
}

fn read_file_or_empty(path: &str) -> String {
    fs::read_to_string(path).unwrap_or_default()
}
