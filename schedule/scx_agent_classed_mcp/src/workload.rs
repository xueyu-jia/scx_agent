use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};

use crate::control_wire::RuleClass;

use super::sha256_hex;

const MAX_PROC_PROCESSES: usize = 131_072;
const MAX_PROC_TASKS: usize = 262_144;
const MAX_MATCHED_TASKS: usize = 65_536;
const MAX_PROC_FILE_BYTES: u64 = 4096;
const MAX_PROC_SCAN_DURATION: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct TaskIdentity {
    tid: u32,
    starttime: u64,
    comm: String,
}

pub struct WorkloadFingerprint {
    pub digest: String,
    pub task_count: usize,
}

pub fn workload_fingerprint(
    proc_root: &Path,
    targets: &[(String, RuleClass)],
) -> Result<WorkloadFingerprint> {
    let started = Instant::now();
    let target_comms = targets
        .iter()
        .map(|(comm, _)| comm.as_str())
        .collect::<BTreeSet<_>>();
    let mut matched_by_comm = targets
        .iter()
        .map(|(comm, _)| (comm.as_str(), 0usize))
        .collect::<BTreeMap<_, _>>();
    let mut identities = BTreeSet::new();
    let mut process_count = 0usize;
    let mut task_count = 0usize;

    for process in fs::read_dir(proc_root)
        .with_context(|| format!("reading proc root '{}'", proc_root.display()))?
    {
        let process = match process {
            Ok(process) => process,
            Err(error) if transient_proc_error(&error) => continue,
            Err(error) => return Err(error).context("enumerating proc processes"),
        };
        let Some(_pid) = numeric_name(&process.file_name()) else {
            continue;
        };
        process_count += 1;
        if process_count > MAX_PROC_PROCESSES {
            bail!("proc scan exceeds {MAX_PROC_PROCESSES} processes");
        }
        if started.elapsed() > MAX_PROC_SCAN_DURATION {
            bail!("proc scan exceeds its 3 second time limit");
        }

        let task_dir = process.path().join("task");
        let tasks = match fs::read_dir(&task_dir) {
            Ok(tasks) => tasks,
            Err(error) if transient_proc_error(&error) => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("reading task directory '{}'", task_dir.display()))
            }
        };
        for task in tasks {
            let task = match task {
                Ok(task) => task,
                Err(error) if transient_proc_error(&error) => continue,
                Err(error) => return Err(error).context("enumerating proc tasks"),
            };
            let Some(tid) = numeric_name(&task.file_name()) else {
                continue;
            };
            task_count += 1;
            if task_count > MAX_PROC_TASKS {
                bail!("proc scan exceeds {MAX_PROC_TASKS} tasks");
            }
            if task_count & 255 == 0 && started.elapsed() > MAX_PROC_SCAN_DURATION {
                bail!("proc scan exceeds its 3 second time limit");
            }

            let Some(mut comm_bytes) = read_proc_file(&task.path().join("comm"))? else {
                continue;
            };
            if comm_bytes.last() == Some(&b'\n') {
                comm_bytes.pop();
            }
            let Ok(comm) = std::str::from_utf8(&comm_bytes) else {
                continue;
            };
            if !target_comms.contains(comm) {
                continue;
            }
            let Some(stat_bytes) = read_proc_file(&task.path().join("stat"))? else {
                continue;
            };
            let stat = std::str::from_utf8(&stat_bytes).context("proc task stat is not UTF-8")?;
            let starttime = parse_stat_starttime(stat)?;
            identities.insert(TaskIdentity {
                tid,
                starttime,
                comm: comm.to_string(),
            });
            *matched_by_comm
                .get_mut(comm)
                .expect("target comm map contains every selected comm") += 1;
            if identities.len() > MAX_MATCHED_TASKS {
                bail!("proc scan exceeds {MAX_MATCHED_TASKS} matching tasks");
            }
        }
    }

    let missing = matched_by_comm
        .iter()
        .filter_map(|(comm, count)| (*count == 0).then_some(*comm))
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        bail!(
            "no live tasks found for target comms: {}",
            missing.join(", ")
        );
    }
    let identities = identities.into_iter().collect::<Vec<_>>();
    Ok(WorkloadFingerprint {
        digest: canonical_workload_digest(targets, &identities),
        task_count: identities.len(),
    })
}

fn numeric_name(name: &std::ffi::OsStr) -> Option<u32> {
    name.to_str()?.parse().ok()
}

fn read_proc_file(path: &Path) -> Result<Option<Vec<u8>>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if transient_proc_error(&error) => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("opening '{}'", path.display())),
    };
    let mut bytes = Vec::new();
    if let Err(error) = file.take(MAX_PROC_FILE_BYTES + 1).read_to_end(&mut bytes) {
        if transient_proc_error(&error) {
            return Ok(None);
        }
        return Err(error).with_context(|| format!("reading '{}'", path.display()));
    }
    if bytes.len() as u64 > MAX_PROC_FILE_BYTES {
        bail!(
            "proc file '{}' exceeds {MAX_PROC_FILE_BYTES} bytes",
            path.display()
        );
    }
    Ok(Some(bytes))
}

fn transient_proc_error(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(3)
}

fn parse_stat_starttime(stat: &str) -> Result<u64> {
    let open = stat
        .find('(')
        .ok_or_else(|| anyhow!("proc task stat is missing comm"))?;
    let close = stat
        .rfind(')')
        .filter(|close| *close > open)
        .ok_or_else(|| anyhow!("proc task stat has an unterminated comm"))?;
    stat[..open]
        .trim()
        .parse::<u32>()
        .context("proc task stat has an invalid tid")?;
    stat[close + 1..]
        .split_whitespace()
        .nth(19)
        .ok_or_else(|| anyhow!("proc task stat is missing starttime"))?
        .parse::<u64>()
        .context("proc task stat has an invalid starttime")
}

fn canonical_workload_digest(targets: &[(String, RuleClass)], tasks: &[TaskIdentity]) -> String {
    let mut targets = targets.to_vec();
    targets.sort_by(|left, right| left.0.cmp(&right.0));
    let mut tasks = tasks.to_vec();
    tasks.sort();

    let mut hasher = Sha256::new();
    hasher.update(b"scx_agent_classed/workload/v1\0");
    for (comm, class) in targets {
        hash_field(&mut hasher, comm.as_bytes());
        hash_field(
            &mut hasher,
            match class {
                RuleClass::Latency => b"latency",
                RuleClass::Batch => b"batch",
            },
        );
    }
    hasher.update(b"\0tasks\0");
    for task in tasks {
        hasher.update(task.tid.to_le_bytes());
        hasher.update(task.starttime.to_le_bytes());
        hash_field(&mut hasher, task.comm.as_bytes());
    }
    let digest = hasher.finalize();
    format!("sha256:{}", sha256_hex(&digest))
}

fn hash_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u32).to_le_bytes());
    hasher.update(value);
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn stat_parser_handles_spaces_and_parentheses_in_comm() {
        let stat = "123 (worker ) name) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 987654 20";
        assert_eq!(parse_stat_starttime(stat).unwrap(), 987654);
        assert!(parse_stat_starttime("123 broken").is_err());
    }

    #[test]
    fn digest_is_canonical_and_includes_target_spec() {
        let first_targets = vec![
            ("worker-b".into(), RuleClass::Batch),
            ("worker-a".into(), RuleClass::Latency),
        ];
        let second_targets = vec![first_targets[1].clone(), first_targets[0].clone()];
        let first_tasks = vec![
            TaskIdentity {
                tid: 22,
                starttime: 200,
                comm: "worker-b".into(),
            },
            TaskIdentity {
                tid: 11,
                starttime: 100,
                comm: "worker-a".into(),
            },
        ];
        let second_tasks = vec![first_tasks[1].clone(), first_tasks[0].clone()];
        assert_eq!(
            canonical_workload_digest(&first_targets, &first_tasks),
            canonical_workload_digest(&second_targets, &second_tasks)
        );

        let mut changed_spec = second_targets;
        changed_spec[0].1 = RuleClass::Batch;
        assert_ne!(
            canonical_workload_digest(&first_targets, &first_tasks),
            canonical_workload_digest(&changed_spec, &first_tasks)
        );
    }

    #[test]
    fn scan_reads_live_task_identity_from_proc_layout() {
        let root = temporary_path("proc");
        let task = root.join("100/task/101");
        fs::create_dir_all(&task).unwrap();
        fs::write(task.join("comm"), b"worker\n").unwrap();
        fs::write(
            task.join("stat"),
            b"101 (worker) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 4242 20\n",
        )
        .unwrap();
        let targets = vec![("worker".into(), RuleClass::Latency)];
        let observed = workload_fingerprint(&root, &targets).unwrap();
        assert_eq!(observed.task_count, 1);
        assert_eq!(
            observed.digest,
            canonical_workload_digest(
                &targets,
                &[TaskIdentity {
                    tid: 101,
                    starttime: 4242,
                    comm: "worker".into()
                }]
            )
        );
        fs::remove_dir_all(root).unwrap();
    }

    fn temporary_path(label: &str) -> PathBuf {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "scx-agent-classed-mcp-{label}-{}-{now}",
            std::process::id()
        ))
    }
}
