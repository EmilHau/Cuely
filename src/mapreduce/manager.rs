use super::{Error, Result, Worker};
use super::{Map, Reduce};
use crate::exponential_backoff::ExponentialBackoff;
use crate::mapreduce::Task;
use indicatif::{ParallelProgressIterator, ProgressBar, ProgressStyle};
use rayon::iter::ParallelBridge;
use rayon::prelude::*;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::io::{Read, Write};
use std::net::ToSocketAddrs;
use std::net::{SocketAddr, TcpStream};
use std::ops::Deref;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{debug, warn};

#[derive(Debug)]
struct RemoteWorker {
    addr: SocketAddr,
}
pub const BUF_SIZE: usize = 4096;

pub const END_OF_MESSAGE: [u8; BUF_SIZE] = [42; BUF_SIZE];

impl RemoteWorker {
    fn retry_strategy() -> impl Iterator<Item = Duration> {
        ExponentialBackoff::from_millis(10).take(5)
    }

    fn connect(&self) -> Result<TcpStream> {
        debug!("connecting to {:}", self.addr);
        for dur in RemoteWorker::retry_strategy() {
            if let Ok(stream) = TcpStream::connect(&self.addr) {
                debug!("connected");
                return Ok(stream);
            }

            std::thread::sleep(dur);
        }

        Err(Error::NoResponse)
    }

    fn perform<W, I, O>(&self, job: &I) -> Result<O>
    where
        W: Worker,
        I: Map<W, O> + Send,
        O: Serialize + DeserializeOwned + Send,
    {
        let mut stream = self.connect()?;
        let serialized_job = bincode::serialize(&Task::Job(job))?;
        debug!("sending {:?} bytes", serialized_job.len());
        stream.write_all(&serialized_job)?;
        stream.write_all(&END_OF_MESSAGE)?;

        let mut buf = [0; BUF_SIZE];
        let mut bytes = Vec::new();
        loop {
            if let Ok(size) = stream.read(&mut buf) {
                debug!("read {:?} bytes", size);
                if size == 0 && bytes.is_empty() {
                    return Err(Error::NoResponse);
                }
                bytes.extend_from_slice(&buf[..size]);

                if bytes.len() >= END_OF_MESSAGE.len()
                    && bytes[bytes.len() - END_OF_MESSAGE.len()..] == END_OF_MESSAGE
                {
                    break;
                }
            }
        }
        bytes = bytes[..bytes.len() - END_OF_MESSAGE.len()].to_vec();
        debug!("finished reading {:?} bytes", bytes.len());

        Ok(bincode::deserialize(&bytes)?)
    }

    fn stop<W, I, O>(&self) -> Result<()>
    where
        W: Worker,
        I: Map<W, O> + Send,
        O: Serialize + DeserializeOwned + Send,
    {
        debug!("closing worker {:}", self.addr);
        let mut stream = self.connect()?;
        let serialized_job = bincode::serialize(&Task::<I>::AllFinished)?;
        debug!("sending {:?} bytes", serialized_job.len());
        stream.write_all(&serialized_job)?;
        stream.write_all(&END_OF_MESSAGE)?;
        Ok(())
    }
}

struct WorkerGuard<'a> {
    from_pool: &'a WorkerPool,
    worker: Arc<RemoteWorker>,
}

impl<'a> WorkerGuard<'a> {
    fn new(pool: &'a WorkerPool, worker: Arc<RemoteWorker>) -> Self {
        Self {
            worker,
            from_pool: pool,
        }
    }

    fn success(self) {
        self.from_pool.insert(Arc::clone(&self.worker));
    }
}

impl<'a> Deref for WorkerGuard<'a> {
    type Target = Arc<RemoteWorker>;

    fn deref(&self) -> &Self::Target {
        &self.worker
    }
}

impl<'a> Drop for WorkerGuard<'a> {
    fn drop(&mut self) {
        self.from_pool.put_back();
    }
}

struct WorkerPool {
    all_workers: Vec<Arc<RemoteWorker>>,
    ready_workers: Mutex<Vec<Arc<RemoteWorker>>>,
    running_workers: AtomicU32,
}

impl WorkerPool {
    fn new<A>(workers: &[A]) -> Self
    where
        A: ToSocketAddrs + std::fmt::Debug,
    {
        let all_workers: Vec<Arc<RemoteWorker>> = workers
            .iter()
            .flat_map(|addr| {
                addr.to_socket_addrs().unwrap_or_else(|_| {
                    panic!("failed to transform {:?} into a socket address", addr)
                })
            })
            .map(|addr| Arc::new(RemoteWorker { addr }))
            .collect();

        Self {
            ready_workers: Mutex::new(all_workers.clone()),
            all_workers,
            running_workers: AtomicU32::new(0),
        }
    }

    fn put_back(&self) {
        self.running_workers.fetch_sub(1, Ordering::SeqCst);
    }

    fn insert(&self, worker: Arc<RemoteWorker>) {
        self.ready_workers.lock().unwrap().push(worker);
    }

    fn get_worker(&self) -> Result<Option<WorkerGuard<'_>>> {
        let mut ready_workers = self.ready_workers.lock().unwrap();
        if ready_workers.len() as u32 + self.running_workers.load(Ordering::SeqCst) == 0 {
            return Err(Error::NoAvailableWorker);
        }

        if let Some(worker) = ready_workers.pop() {
            self.running_workers.fetch_add(1, Ordering::SeqCst);
            Ok(Some(WorkerGuard::new(self, worker)))
        } else {
            Ok(None)
        }
    }

    fn stop_workers<W, I, O>(&self)
    where
        W: Worker,
        I: Map<W, O> + Send,
        O: Serialize + DeserializeOwned + Send,
    {
        let mut failing_workers = Vec::new();
        for worker in &self.all_workers {
            if worker.stop::<W, I, O>().is_err() {
                failing_workers.push(worker);
            }
        }

        if !failing_workers.is_empty() {
            debug!(
                "failed to stop the following workers: {:#?}",
                failing_workers
            );
        }
    }
}

pub struct Manager {
    pool: WorkerPool,
}

impl Manager {
    pub fn new<A>(workers: &[A]) -> Self
    where
        A: ToSocketAddrs + std::fmt::Debug,
    {
        Self {
            pool: WorkerPool::new(workers),
        }
    }

    fn try_map<W, I, O>(&self, job: &I) -> Result<O>
    where
        W: Worker,
        I: Map<W, O> + Send,
        O: Serialize + DeserializeOwned + Send,
    {
        loop {
            match self.pool.get_worker()? {
                Some(worker) => {
                    let res = worker.perform(job)?;
                    worker.success();

                    return Ok(res);
                }
                None => std::thread::sleep(std::time::Duration::from_millis(1000)),
            }
        }
    }

    /// Execute job on one of the remote machines. If the remote machine fails for some reason,
    /// the job should be allocated to another machine.
    pub fn map<W, I, O>(&self, job: I) -> O
    where
        W: Worker,
        I: Map<W, O> + Send,
        O: Serialize + DeserializeOwned + Send,
    {
        loop {
            match self.try_map(&job) {
                Ok(res) => return res,
                Err(Error::NoAvailableWorker) => panic!("{}", Error::NoAvailableWorker),
                Err(err) => {
                    warn!("Worker failed - rescheduling job");
                    debug!("{:?}", err);
                }
            }
        }
    }

    fn reduce<O1, O2>(acc: Option<O2>, elem: O1) -> O2
    where
        O1: Serialize + DeserializeOwned + Send,
        O2: From<O1> + Reduce<O1> + Send,
    {
        match acc {
            Some(acc) => acc.reduce(elem),
            None => elem.into(),
        }
    }

    fn reduce_end<O>(acc: Option<O>, elem: O) -> O
    where
        O: Reduce<O> + Send,
    {
        match acc {
            Some(acc) => acc.reduce(elem),
            None => elem,
        }
    }

    #[allow(clippy::trait_duplication_in_bounds)]
    fn get_results<W, I, O1, O2>(&self, jobs: impl Iterator<Item = I> + Send) -> Option<O2>
    where
        W: Worker,
        I: Map<W, O1> + Send,
        O1: Serialize + DeserializeOwned + Send,
        O2: From<O1> + Reduce<O1> + Send + Reduce<O2>,
    {
        let acc: Arc<Mutex<Option<O2>>> = Arc::new(Mutex::new(None));

        let size = jobs.size_hint();

        match size.1 {
            Some(size) => {
                let pb = ProgressBar::new(size as u64);
                pb.set_style(
                    ProgressStyle::default_bar()
                        .template(
                            "{spinner:.green} [{elapsed_precise}] [{wide_bar}] {pos:>7}/{len:7} ({eta})",
                        )
                        .progress_chars("#>-"),
                );
                jobs.par_bridge()
                    .map(|job| self.map::<W, I, O1>(job))
                    .progress_with(pb)
                    .fold(
                        || None,
                        |acc: Option<O2>, elem| Some(Manager::reduce(acc, elem)),
                    )
                    .for_each(|res| {
                        if let Some(res) = res {
                            let mut lock = acc.lock().unwrap();
                            *lock = Some(Manager::reduce_end(lock.take(), res));
                        }
                    });
            }
            None => {
                jobs.par_bridge()
                    .map(|job| self.map::<W, I, O1>(job))
                    .fold(
                        || None,
                        |acc: Option<O2>, elem| Some(Manager::reduce(acc, elem)),
                    )
                    .for_each(|res| {
                        if let Some(res) = res {
                            let mut lock = acc.lock().unwrap();
                            *lock = Some(Manager::reduce_end(lock.take(), res));
                        }
                    });
            }
        }

        let x = acc.lock().unwrap().take();
        x
    }

    #[allow(clippy::trait_duplication_in_bounds)]
    pub fn run<W, I, O1, O2>(self, jobs: impl Iterator<Item = I> + Send) -> Option<O2>
    where
        W: Worker,
        I: Map<W, O1> + Send,
        O1: Serialize + DeserializeOwned + Send,
        O2: From<O1> + Reduce<O1> + Send + Reduce<O2>,
    {
        let result = self.get_results(jobs);
        self.pool.stop_workers::<W, I, O1>();

        result
    }
}
