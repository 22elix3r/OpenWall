// SPDX-License-Identifier: AGPL-3.0-or-later
//! Bounded process accounting for the private native video diagnostic.
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::{
    io::Read,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Serialize)]
pub struct Counters {
    pub submissions: u64,
    pub presentation_feedbacks: u64,
    pub discarded_feedbacks: u64,
    pub received_samples: u64,
    pub replaced_samples: u64,
    pub loops: u64,
}

impl Counters {
    fn since(self, start: Self) -> Self {
        Self {
            submissions: self.submissions - start.submissions,
            presentation_feedbacks: self.presentation_feedbacks - start.presentation_feedbacks,
            discarded_feedbacks: self.discarded_feedbacks - start.discarded_feedbacks,
            received_samples: self.received_samples - start.received_samples,
            replaced_samples: self.replaced_samples - start.replaced_samples,
            loops: self.loops - start.loops,
        }
    }
}

fn cpu_seconds() -> f64 {
    let time = rustix::time::clock_gettime(rustix::time::ClockId::ProcessCPUTime);
    time.tv_sec as f64 + time.tv_nsec as f64 / 1_000_000_000.0
}

fn rss_bytes() -> Result<u64> {
    let mut text = String::new();
    std::fs::File::open("/proc/self/status")?
        .take(16_384)
        .read_to_string(&mut text)?;
    let line = text
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .context("process RSS missing")?;
    let mut fields = line.split_whitespace();
    let kib: u64 = fields
        .next()
        .context("process RSS value missing")?
        .parse()?;
    ensure!(fields.next() == Some("kB"), "unexpected process RSS unit");
    kib.checked_mul(1024).context("process RSS overflow")
}

struct Sample {
    at: Instant,
    cpu: f64,
    counters: Counters,
    rss_sum: u128,
    rss_peak: u64,
    rss_samples: u64,
    next_rss: Instant,
}

pub struct Run {
    seconds: Option<u64>,
    warmup: Duration,
    warmup_end: Option<Instant>,
    sample: Option<Sample>,
    result: Option<serde_json::Value>,
}

impl Run {
    pub fn new(seconds: Option<u64>, warmup: u64) -> Self {
        Self {
            seconds,
            warmup: Duration::from_secs(warmup),
            warmup_end: None,
            sample: None,
            result: None,
        }
    }

    pub fn tick(&mut self, ready: bool, counters: Counters) -> Result<bool> {
        let Some(seconds) = self.seconds else {
            return Ok(false);
        };
        let now = Instant::now();
        if !ready {
            return Ok(false);
        }
        let warmup_end = *self.warmup_end.get_or_insert(now + self.warmup);
        if now < warmup_end {
            return Ok(false);
        }
        if self.sample.is_none() {
            let rss = rss_bytes()?;
            self.sample = Some(Sample {
                at: now,
                cpu: cpu_seconds(),
                counters,
                rss_sum: u128::from(rss),
                rss_peak: rss,
                rss_samples: 1,
                next_rss: now + Duration::from_millis(250),
            });
            eprintln!("Native GL measurement started after actual presentation and warmup");
        }
        let sample = self.sample.as_mut().context("measurement sample missing")?;
        let elapsed = now.duration_since(sample.at);
        if now >= sample.next_rss || elapsed >= Duration::from_secs(seconds) {
            let rss = rss_bytes()?;
            sample.rss_sum += u128::from(rss);
            sample.rss_peak = sample.rss_peak.max(rss);
            sample.rss_samples += 1;
            sample.next_rss = now + Duration::from_millis(250);
        }
        if elapsed < Duration::from_secs(seconds) {
            return Ok(false);
        }
        let elapsed = elapsed.as_secs_f64();
        let observed = counters.since(sample.counters);
        ensure!(
            observed.submissions > 0 && observed.presentation_feedbacks > 0,
            "measurement contains no native video submissions/presentation feedback"
        );
        self.result = Some(serde_json::json!({"scope":"video_process_only",
            "sample_seconds":elapsed,"requested_sample_seconds":seconds,"warmup_seconds":self.warmup.as_secs(),
            "cpu_percent_one_core":(cpu_seconds()-sample.cpu)/elapsed*100.0,
            "mean_sampled_rss_mib":sample.rss_sum as f64/sample.rss_samples as f64/1048576.0,
            "peak_sampled_rss_mib":sample.rss_peak as f64/1048576.0,"rss_samples":sample.rss_samples,
            "observed":observed,"presentation_feedback_hz":observed.presentation_feedbacks as f64/elapsed}));
        Ok(true)
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        self.sample
            .as_ref()
            .map(|sample| {
                sample
                    .next_rss
                    .min(sample.at + Duration::from_secs(self.seconds.unwrap_or(0)))
            })
            .or(self.warmup_end)
    }

    pub fn result(&self) -> Result<Option<&serde_json::Value>> {
        ensure!(
            self.seconds.is_none() || self.result.is_some(),
            "native measurement interrupted before completion"
        );
        Ok(self.result.as_ref())
    }
}
