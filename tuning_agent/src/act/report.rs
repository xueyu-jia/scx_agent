use serde_json::Value;

#[derive(Clone, Debug)]
pub struct ExecutionReport {
    pub command: String,
    pub status: Option<i32>,
    pub timed_out: bool,
    pub stdout: String,
    pub stderr: String,
}

impl ExecutionReport {
    pub fn succeeded(&self) -> bool {
        !self.timed_out && self.status == Some(0)
    }

    pub fn to_json_value(&self, limit: usize) -> Value {
        let stdout = truncate(&self.stdout, limit);
        let stderr = truncate(&self.stderr, limit);
        serde_json::json!({
            "command": self.command,
            "status": self.status,
            "timed_out": self.timed_out,
            "stdout": stdout,
            "stderr": stderr,
            "stdout_truncated": stdout.len() < self.stdout.len(),
            "stderr_truncated": stderr.len() < self.stderr.len(),
        })
    }
}

fn truncate(input: &str, limit: usize) -> String {
    if input.len() <= limit {
        return input.to_string();
    }

    let mut end = limit;
    while !input.is_char_boundary(end) {
        end -= 1;
    }
    input[..end].to_string()
}
