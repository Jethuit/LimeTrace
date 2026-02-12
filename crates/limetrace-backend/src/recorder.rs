use anyhow::Result;
use std::time::Duration;

use crate::db::{Database, SegmentInsert};
use crate::monitor::{ActivityKind, ActivitySample};

#[derive(Debug, Clone, PartialEq, Eq)]
struct SegmentKey {
    app_id: Option<i64>,
    title_id: Option<i64>,
    is_idle: bool,
    pid: Option<u32>,
    pid_create_time: Option<u64>,
}

#[derive(Debug, Clone)]
struct OpenSegment {
    start_ts: i64,
    end_ts: i64,
    key: SegmentKey,
}

pub struct Recorder {
    db: Database,
    current: Option<OpenSegment>,
    rotate_every_secs: i64,
}

impl Recorder {
    pub fn new(db: Database, rotate_every: Duration) -> Self {
        Self {
            db,
            current: None,
            rotate_every_secs: rotate_every.as_secs() as i64,
        }
    }

    pub fn ingest(&mut self, sample: ActivitySample) -> Result<()> {
        let sample_ts = sample.ts;
        let (key, segment_start_ts, trim_active_after_ts) = match &sample.kind {
            ActivityKind::Idle { idle_ms } => {
                let idle_secs = i64::from(*idle_ms / 1000);
                let idle_start_ts = sample_ts.saturating_sub(idle_secs);
                (Self::idle_key(), idle_start_ts, Some(idle_start_ts))
            }
            ActivityKind::Active(_) => (self.build_key(&sample)?, sample_ts, None),
        };

        if let Some(cutoff_ts) = trim_active_after_ts {
            self.db.truncate_active_segments_from(cutoff_ts)?;
        }

        if self
            .current
            .as_ref()
            .map(|current| current.key == key)
            .unwrap_or(false)
        {
            let should_rotate = {
                if let Some(current) = self.current.as_mut() {
                    current.end_ts = sample_ts;
                    self.rotate_every_secs > 0
                        && current.end_ts.saturating_sub(current.start_ts) >= self.rotate_every_secs
                } else {
                    false
                }
            };

            if should_rotate {
                if let Some(flushed) = self.current.take() {
                    self.flush_segment(&flushed)?;
                    self.current = Some(OpenSegment {
                        start_ts: sample_ts,
                        end_ts: sample_ts,
                        key,
                    });
                }
            }
            return Ok(());
        }

        if let Some(previous) = self.current.take() {
            let mut previous = previous;
            // If we just detected idle, trim the tail of the in-memory active segment
            // before flushing it, so the cutoff can become idle.
            if key.is_idle && !previous.key.is_idle {
                previous.end_ts = previous.end_ts.min(segment_start_ts);
            }
            self.flush_segment(&previous)?;
        }

        self.current = Some(OpenSegment {
            start_ts: segment_start_ts,
            end_ts: sample_ts,
            key,
        });
        Ok(())
    }

    pub fn flush_and_close(&mut self, now_ts: i64) -> Result<()> {
        if let Some(mut current) = self.current.take() {
            if now_ts > current.end_ts {
                current.end_ts = now_ts;
            }
            self.flush_segment(&current)?;
        }
        Ok(())
    }

    fn build_key(&mut self, sample: &ActivitySample) -> Result<SegmentKey> {
        match &sample.kind {
            ActivityKind::Idle { .. } => Ok(Self::idle_key()),
            ActivityKind::Active(active) => {
                let app_id = self.db.upsert_app(&active.exe_name, &active.process_path)?;
                let title_id = if active.window_title.is_empty() {
                    None
                } else {
                    Some(self.db.upsert_title(&active.window_title)?)
                };

                Ok(SegmentKey {
                    app_id: Some(app_id),
                    title_id,
                    is_idle: false,
                    pid: Some(active.pid),
                    pid_create_time: active.pid_create_time,
                })
            }
        }
    }

    fn idle_key() -> SegmentKey {
        SegmentKey {
            app_id: None,
            title_id: None,
            is_idle: true,
            pid: None,
            pid_create_time: None,
        }
    }

    fn flush_segment(&mut self, segment: &OpenSegment) -> Result<()> {
        let row = SegmentInsert {
            start_ts: segment.start_ts,
            end_ts: segment.end_ts,
            app_id: segment.key.app_id,
            title_id: segment.key.title_id,
            is_idle: segment.key.is_idle,
            pid: segment.key.pid,
            pid_create_time: segment.key.pid_create_time,
        };
        self.db.insert_segment(&row)
    }
}
