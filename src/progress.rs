use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};

#[derive(Debug)]
pub struct ProgressReporter {
    started_at: Instant,
    agent_total: AtomicU64,
    agent_started: AtomicU64,
    agent_finished: AtomicU64,
    multi: MultiProgress,
    session_log: Mutex<std::fs::File>,
}

impl ProgressReporter {
    pub fn new(session_log_path: PathBuf) -> Result<Self> {
        let session_log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&session_log_path)
            .with_context(|| format!("failed to open {}", session_log_path.display()))?;

        let multi = MultiProgress::with_draw_target(ProgressDrawTarget::stderr_with_hz(10));
        Ok(Self {
            started_at: Instant::now(),
            agent_total: AtomicU64::new(0),
            agent_started: AtomicU64::new(0),
            agent_finished: AtomicU64::new(0),
            multi,
            session_log: Mutex::new(session_log),
        })
    }

    pub fn info(&self, area: &'static str, message: impl AsRef<str>) {
        let message = message.as_ref();
        self.emit_log(area, "INFO", message);
        let _ = self.multi.println(format!(
            "INFO  {:<7} {}",
            area.to_ascii_uppercase(),
            message
        ));
    }

    pub fn summary(&self, status: &'static str, message: impl AsRef<str>) {
        let message = message.as_ref();
        self.emit_log("run", status, message);
        let _ = self
            .multi
            .println(format!("{} {}", status.to_ascii_uppercase(), message));
    }

    pub fn log_block(&self, title: &str, body: &str) {
        let mut log = self.session_log.lock().expect("session log mutex poisoned");
        let _ = writeln!(log, "\n===== {} =====\n{}\n", title, body);
        let _ = log.flush();
    }

    pub fn begin_step(
        self: &Arc<Self>,
        area: &'static str,
        label: impl Into<String>,
    ) -> StepHandle {
        let label = label.into();
        self.emit_log(area, "START", &label);
        let bar = self.new_spinner(area, &label);
        StepHandle {
            reporter: self.clone(),
            area,
            label,
            started_at: Instant::now(),
            completed: false,
            bar,
        }
    }

    pub fn set_agent_total(&self, total: usize) {
        self.agent_total.store(total as u64, Ordering::Relaxed);
        self.agent_started.store(0, Ordering::Relaxed);
        self.agent_finished.store(0, Ordering::Relaxed);
        self.emit_log(
            "agents",
            "PLAN",
            &format!("{total} provider invocations queued"),
        );
        let _ = self
            .multi
            .println(format!("PLAN  AGENTS  {total} provider invocations queued"));
    }

    pub fn begin_agent(self: &Arc<Self>, label: impl Into<String>) -> AgentHandle {
        let label = label.into();
        let ordinal = self.agent_started.fetch_add(1, Ordering::Relaxed) + 1;
        let total = self.agent_total.load(Ordering::Relaxed);
        let display_label = format!("[{ordinal}/{total}] {label}");
        self.emit_log("agent", "START", &display_label);
        let bar = self.new_spinner("agent", &display_label);
        AgentHandle {
            reporter: self.clone(),
            label: display_label,
            started_at: Instant::now(),
            completed: false,
            bar,
        }
    }

    pub fn begin_command(self: &Arc<Self>, label: impl Into<String>) -> CommandHandle {
        let label = label.into();
        self.emit_log("cmd", "START", &label);
        let bar = self.new_spinner("cmd", &label);
        CommandHandle {
            reporter: self.clone(),
            label,
            started_at: Instant::now(),
            bar,
        }
    }

    fn new_spinner(&self, area: &'static str, label: &str) -> ProgressBar {
        let bar = self.multi.add(ProgressBar::new_spinner());
        bar.set_style(spinner_style());
        bar.set_prefix(area.to_ascii_uppercase());
        bar.set_message(label.to_string());
        bar.enable_steady_tick(Duration::from_millis(120));
        bar
    }

    fn emit_log(&self, area: &'static str, status: &str, message: &str) {
        let mut log = self.session_log.lock().expect("session log mutex poisoned");
        let _ = writeln!(
            log,
            "[{:>6.1}s] {:<7} {:<6} {}",
            self.started_at.elapsed().as_secs_f32(),
            area.to_ascii_uppercase(),
            status,
            message
        );
        let _ = log.flush();
    }
}

pub struct StepHandle {
    reporter: Arc<ProgressReporter>,
    area: &'static str,
    label: String,
    started_at: Instant,
    completed: bool,
    bar: ProgressBar,
}

impl StepHandle {
    pub fn done(mut self, detail: impl AsRef<str>) {
        self.completed = true;
        let message = format!(
            "{} ({:.1}s){}",
            self.label,
            self.started_at.elapsed().as_secs_f32(),
            render_detail(detail.as_ref())
        );
        self.reporter.emit_log(self.area, "DONE", &message);
        self.bar.finish_with_message(message);
    }

    pub fn fail(mut self, detail: impl AsRef<str>) {
        self.completed = true;
        let message = format!(
            "FAILED {} ({:.1}s){}",
            self.label,
            self.started_at.elapsed().as_secs_f32(),
            render_detail(detail.as_ref())
        );
        self.reporter.emit_log(self.area, "FAIL", &message);
        self.bar.abandon_with_message(message);
    }
}

impl Drop for StepHandle {
    fn drop(&mut self) {
        if !self.completed && !std::thread::panicking() {
            self.reporter.emit_log(self.area, "ABORT", &self.label);
            self.bar
                .abandon_with_message(format!("ABORTED {}", self.label));
        }
    }
}

pub struct AgentHandle {
    reporter: Arc<ProgressReporter>,
    label: String,
    started_at: Instant,
    completed: bool,
    bar: ProgressBar,
}

impl AgentHandle {
    pub fn done(mut self) {
        self.completed = true;
        let finished = self.reporter.agent_finished.fetch_add(1, Ordering::Relaxed) + 1;
        let total = self.reporter.agent_total.load(Ordering::Relaxed);
        let message = format!(
            "[{finished}/{total}] {} ({:.1}s)",
            self.label,
            self.started_at.elapsed().as_secs_f32()
        );
        self.reporter.emit_log("agent", "DONE", &message);
        self.bar.finish_with_message(message);
    }

    pub fn fail(mut self, detail: impl AsRef<str>) {
        self.completed = true;
        let finished = self.reporter.agent_finished.fetch_add(1, Ordering::Relaxed) + 1;
        let total = self.reporter.agent_total.load(Ordering::Relaxed);
        let message = format!(
            "[{finished}/{total}] {} ({:.1}s){}",
            self.label,
            self.started_at.elapsed().as_secs_f32(),
            render_detail(detail.as_ref())
        );
        self.reporter.emit_log("agent", "FAIL", &message);
        self.bar.abandon_with_message(format!("FAILED {message}"));
    }
}

impl Drop for AgentHandle {
    fn drop(&mut self) {
        if !self.completed && !std::thread::panicking() {
            self.reporter.emit_log("agent", "ABORT", &self.label);
            self.bar
                .abandon_with_message(format!("ABORTED {}", self.label));
        }
    }
}

#[derive(Clone)]
pub struct CommandHandle {
    reporter: Arc<ProgressReporter>,
    label: String,
    started_at: Instant,
    bar: ProgressBar,
}

impl CommandHandle {
    pub fn heartbeat(&self, elapsed_secs: f32) {
        let message = format!("{} ({elapsed_secs:.1}s elapsed)", self.label);
        self.reporter.emit_log("cmd", "INFO", &message);
    }

    pub fn done(&self, detail: impl AsRef<str>) {
        let message = format!(
            "{} ({:.1}s){}",
            self.label,
            self.started_at.elapsed().as_secs_f32(),
            render_detail(detail.as_ref())
        );
        self.reporter.emit_log("cmd", "DONE", &message);
        self.bar.finish_with_message(message);
    }

    pub fn fail(&self, detail: impl AsRef<str>) {
        let message = format!(
            "{} ({:.1}s){}",
            self.label,
            self.started_at.elapsed().as_secs_f32(),
            render_detail(detail.as_ref())
        );
        self.reporter.emit_log("cmd", "FAIL", &message);
        self.bar.abandon_with_message(format!("FAILED {message}"));
    }
}

fn spinner_style() -> ProgressStyle {
    ProgressStyle::with_template("{spinner} {prefix:>7} [{elapsed_precise}] {msg}")
        .expect("valid indicatif spinner template")
        .tick_strings(&["   ", ".  ", ".. ", "...", " ..", "  .", "   "])
}

fn render_detail(detail: &str) -> String {
    if detail.trim().is_empty() {
        String::new()
    } else {
        format!(" -> {}", detail.trim())
    }
}
