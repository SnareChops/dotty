use crate::registry::{ConvertCtx, Registry};
use crate::{output, Edits, Error, Format, Options};
use crossbeam_channel::{unbounded, Receiver, Sender};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

pub type JobId = u64;

#[derive(Debug, Clone)]
pub struct JobRequest {
    pub id: JobId,
    pub src: PathBuf,
    pub from: Format,
    pub to: Format,
    pub options: Options,
    /// Editor changes to apply before encoding.
    pub edits: Option<Arc<Edits>>,
}

#[derive(Debug)]
pub enum JobEvent {
    Started(JobId),
    Progress(JobId, f32),
    /// Finished; the strings are notes about anything lossy or surprising.
    Done(JobId, PathBuf, Vec<String>),
    Failed(JobId, String),
    Cancelled(JobId),
}

/// Runs jobs on a fixed worker pool. Events are delivered on `events()`;
/// `wake` is called after each event so a UI can request a repaint.
pub struct JobRunner {
    tx: Sender<JobRequest>,
    events: Receiver<JobEvent>,
    cancel: Arc<AtomicBool>,
}

impl JobRunner {
    pub fn new(registry: Arc<Registry>, workers: usize, wake: impl Fn() + Send + Sync + 'static) -> Self {
        let (tx, rx) = unbounded::<JobRequest>();
        let (etx, events) = unbounded();
        let cancel = Arc::new(AtomicBool::new(false));
        let wake = Arc::new(wake);

        for _ in 0..workers.max(1) {
            let (rx, etx, registry, cancel, wake) =
                (rx.clone(), etx.clone(), registry.clone(), cancel.clone(), wake.clone());
            thread::spawn(move || {
                for job in rx {
                    let send = |e: JobEvent| {
                        let _ = etx.send(e);
                        wake();
                    };
                    if cancel.load(Ordering::Relaxed) {
                        send(JobEvent::Cancelled(job.id));
                        continue;
                    }
                    send(JobEvent::Started(job.id));
                    // A codec bug on one file must not take down the app.
                    let result = catch_unwind(AssertUnwindSafe(|| run_job(&job, &registry, &cancel, &send)))
                        .unwrap_or_else(|_| Err(Error::Codec("internal error while converting".into())));
                    send(match result {
                        Ok((path, notes)) => JobEvent::Done(job.id, path, notes),
                        Err(Error::Cancelled) => JobEvent::Cancelled(job.id),
                        Err(e) => JobEvent::Failed(job.id, e.to_string()),
                    });
                }
            });
        }
        Self { tx, events, cancel }
    }

    pub fn submit(&self, job: JobRequest) {
        self.cancel.store(false, Ordering::Relaxed);
        let _ = self.tx.send(job);
    }

    /// Cancel all running and queued jobs.
    pub fn cancel_all(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn events(&self) -> &Receiver<JobEvent> {
        &self.events
    }
}

fn run_job(
    job: &JobRequest,
    registry: &Registry,
    cancel: &AtomicBool,
    send: &dyn Fn(JobEvent),
) -> crate::Result<(PathBuf, Vec<String>)> {
    let converter = registry
        .find(job.from, job.to)
        .ok_or(Error::NoRoute { from: job.from, to: job.to })?;
    let dst = output::output_path(&job.src, job.to, &job.options);
    let progress = |p: f32| send(JobEvent::Progress(job.id, p));
    let ctx = ConvertCtx::new(&job.options, cancel, &progress).with_edits(job.edits.as_deref());
    if let Err(e) = converter.convert(&job.src, &dst, job.from, job.to, &ctx) {
        let _ = std::fs::remove_file(&dst); // don't leave partial output
        return Err(e);
    }
    Ok((ctx.take_output().unwrap_or(dst), ctx.take_notes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::Converter;
    use std::path::Path;
    use std::time::Duration;

    struct Copy;
    impl Converter for Copy {
        fn routes(&self) -> Vec<(Format, Format)> {
            vec![(Format::Png, Format::Jpeg)]
        }
        fn convert(&self, src: &Path, dst: &Path, _: Format, _: Format, ctx: &ConvertCtx) -> crate::Result<()> {
            (ctx.progress)(0.5);
            ctx.note("copied");
            ctx.note("copied"); // duplicates collapse
            std::fs::copy(src, dst).map(|_| ()).map_err(|source| Error::Io { path: src.into(), source })
        }
    }

    #[test]
    fn runs_job_and_reports_missing_route() {
        let dir = std::env::temp_dir().join(format!("tf-jobs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("a.png");
        std::fs::write(&src, b"x").unwrap();

        let mut reg = Registry::new();
        reg.register(Box::new(Copy));
        let runner = JobRunner::new(Arc::new(reg), 1, || {});
        let mk = |id, to| JobRequest { id, src: src.clone(), from: Format::Png, to, options: Options::default(), edits: None };
        runner.submit(mk(1, Format::Jpeg));
        runner.submit(mk(2, Format::Wav));

        let mut done = false;
        let mut failed = false;
        while !(done && failed) {
            match runner.events().recv_timeout(Duration::from_secs(5)).unwrap() {
                JobEvent::Done(1, p, notes) => {
                    assert_eq!(p, dir.join("a.jpg"));
                    assert_eq!(notes, vec!["copied".to_string()]);
                    done = true;
                }
                JobEvent::Failed(2, msg) => {
                    assert!(msg.contains("no converter"));
                    failed = true;
                }
                _ => {}
            }
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
