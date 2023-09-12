use crate::deno_runtime::DenoRuntime;
use crate::utils::send_event_if_event_worker_available;
use crate::utils::units::bytes_to_display;

use crate::rt_worker::worker::{Worker, WorkerHandler};
use crate::rt_worker::worker_pool::WorkerPool;
use anyhow::{bail, Error};
use cpu_timer::{CPUAlarmVal, CPUTimer};
use event_worker::events::{BootEvent, PseudoEvent, WorkerEventWithMetadata, WorkerEvents};
use hyper::{Body, Request, Response};
use log::{debug, error};
use sb_worker_context::essentials::{
    EventWorkerRuntimeOpts, UserWorkerMsgs, WorkerContextInitOpts, WorkerRuntimeOpts,
};
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot};

#[derive(Debug)]
pub struct WorkerRequestMsg {
    pub req: Request<Body>,
    pub res_tx: oneshot::Sender<Result<Response<Body>, hyper::Error>>,
}

#[derive(Debug, Clone)]
pub struct UserWorkerProfile {
    pub(crate) worker_event_tx: mpsc::UnboundedSender<WorkerRequestMsg>,
}

async fn handle_request(
    unix_stream_tx: mpsc::UnboundedSender<UnixStream>,
    msg: WorkerRequestMsg,
) -> Result<(), Error> {
    // create a unix socket pair
    let (sender_stream, recv_stream) = UnixStream::pair()?;

    let _ = unix_stream_tx.send(recv_stream);

    // send the HTTP request to the worker over Unix stream
    let (mut request_sender, connection) = hyper::client::conn::handshake(sender_stream).await?;

    // spawn a task to poll the connection and drive the HTTP state
    tokio::task::spawn(async move {
        if let Err(e) = connection.without_shutdown().await {
            error!("Error in worker connection: {}", e);
        }
    });
    tokio::task::yield_now().await;

    let result = request_sender.send_request(msg.req).await;
    let _ = msg.res_tx.send(result);

    Ok(())
}

pub fn create_supervisor(
    key: u64,
    worker_runtime: &mut DenoRuntime,
    termination_event_tx: oneshot::Sender<WorkerEvents>,
) -> Result<CPUTimer, Error> {
    let (memory_limit_tx, mut memory_limit_rx) = mpsc::unbounded_channel::<()>();
    let thread_safe_handle = worker_runtime.js_runtime.v8_isolate().thread_safe_handle();

    // we assert supervisor is only run for user workers
    let conf = worker_runtime.conf.as_user_worker().unwrap().clone();

    worker_runtime.js_runtime.add_near_heap_limit_callback(move |cur, _| {
        debug!(
            "Low memory alert triggered: {}",
            bytes_to_display(cur as u64),
        );

        if memory_limit_tx.send(()).is_err() {
            error!("failed to send memory limit reached notification - isolate may already be terminating");
        };

        // give an allowance on current limit (until the isolate is terminated)
        // we do this so that oom won't end up killing the edge-runtime process
        cur * (conf.low_memory_multiplier as usize)
    });

    // Note: CPU timer must be started in the same thread as the worker runtime
    let (cpu_alarms_tx, mut cpu_alarms_rx) = mpsc::unbounded_channel::<()>();
    let cputimer = CPUTimer::start(conf.cpu_time_threshold_ms, CPUAlarmVal { cpu_alarms_tx })?;

    let thread_name = format!("sb-sup-{:?}", key);
    let _handle = thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let local = tokio::task::LocalSet::new();

            let future = async move {
                let mut bursts = 0;
                let mut last_burst = Instant::now();

                let sleep = tokio::time::sleep(Duration::from_millis(conf.worker_timeout_ms));
                tokio::pin!(sleep);

                loop {
                    tokio::select! {
                        Some(_) = cpu_alarms_rx.recv() => {
                            if last_burst.elapsed().as_millis() > (conf.cpu_burst_interval_ms as u128) {
                                bursts += 1;
                                last_burst = Instant::now();
                            }
                            // at half way of max cpu burst
                            // retire the worker
                            if bursts > conf.max_cpu_bursts {
                                thread_safe_handle.terminate_execution();
                                error!("CPU time limit reached. isolate: {:?}", key);
                                return WorkerEvents::CpuTimeLimit(PseudoEvent{})
                            }
                        }

                        // wall-clock limit
                        // at half way of wall clock limit retire the worker
                        () = &mut sleep => {
                            // use interrupt to capture the heap stats
                            //thread_safe_handle.request_interrupt(callback, std::ptr::null_mut());
                            thread_safe_handle.terminate_execution();
                            error!("wall clock duration reached. isolate: {:?}", key);
                            return WorkerEvents::WallClockTimeLimit(PseudoEvent{});
                        }

                        // memory usage
                        Some(_) = memory_limit_rx.recv() => {
                            thread_safe_handle.terminate_execution();
                            error!("memory limit reached for the worker. isolate: {:?}", key);
                            return WorkerEvents::MemoryLimit(PseudoEvent{});
                        }
                    }
                }
            };

            let result = local.block_on(&rt, future);

            // send termination reason
            let _ = termination_event_tx.send(result);
        })
        .unwrap();

    Ok(cputimer)
}

pub async fn create_worker(
    init_opts: WorkerContextInitOpts,
) -> Result<mpsc::UnboundedSender<WorkerRequestMsg>, Error> {
    let (worker_boot_result_tx, worker_boot_result_rx) = oneshot::channel::<Result<(), Error>>();
    let (unix_stream_tx, unix_stream_rx) = mpsc::unbounded_channel::<UnixStream>();
    let worker_init = Worker::new(&init_opts)?;

    let worker: Box<dyn WorkerHandler> = Box::new(worker_init);

    // Downcast to call the method in "Worker" since the implementation might be of worker
    // But at the end we are using the trait itself.
    // Downcasting it to Worker will give us access to its parent implementation
    let downcast_reference = worker.as_any().downcast_ref::<Worker>();
    if let Some(worker_struct_ref) = downcast_reference {
        worker_struct_ref.start(init_opts, unix_stream_rx, worker_boot_result_tx);

        // create an async task waiting for requests for worker
        let (worker_req_tx, mut worker_req_rx) = mpsc::unbounded_channel::<WorkerRequestMsg>();

        let worker_req_handle: tokio::task::JoinHandle<Result<(), Error>> =
            tokio::task::spawn(async move {
                while let Some(msg) = worker_req_rx.recv().await {
                    let unix_stream_tx_clone = unix_stream_tx.clone();
                    tokio::task::spawn(async move {
                        if let Err(err) = handle_request(unix_stream_tx_clone, msg).await {
                            error!("worker failed to handle request: {:?}", err);
                        }
                    });
                }

                Ok(())
            });

        // wait for worker to be successfully booted
        let worker_boot_result = worker_boot_result_rx.await?;
        match worker_boot_result {
            Err(err) => {
                worker_req_handle.abort();
                bail!(err)
            }
            Ok(_) => {
                let elapsed = worker_struct_ref
                    .worker_boot_start_time
                    .elapsed()
                    .as_millis();
                send_event_if_event_worker_available(
                    worker_struct_ref.events_msg_tx.clone(),
                    WorkerEvents::Boot(BootEvent {
                        boot_time: elapsed as usize,
                    }),
                    worker_struct_ref.event_metadata.clone(),
                );
                Ok(worker_req_tx)
            }
        }
    } else {
        bail!("Unknown")
    }
}

pub async fn send_user_worker_request(
    worker_channel: mpsc::UnboundedSender<WorkerRequestMsg>,
    req: Request<Body>,
) -> Result<Response<Body>, Error> {
    let (res_tx, res_rx) = oneshot::channel::<Result<Response<Body>, hyper::Error>>();
    let msg = WorkerRequestMsg { req, res_tx };

    // send the message to worker
    worker_channel.send(msg)?;

    // wait for the response back from the worker
    let res = res_rx.await??;

    // send the response back to the caller

    Ok(res)
}

pub async fn create_events_worker(
    events_worker_path: PathBuf,
    import_map_path: Option<String>,
    no_module_cache: bool,
) -> Result<mpsc::UnboundedSender<WorkerEventWithMetadata>, Error> {
    let (events_tx, events_rx) = mpsc::unbounded_channel::<WorkerEventWithMetadata>();

    let _ = create_worker(WorkerContextInitOpts {
        service_path: events_worker_path,
        no_module_cache,
        import_map_path,
        env_vars: std::env::vars().collect(),
        events_rx: Some(events_rx),
        maybe_eszip: None,
        maybe_entrypoint: None,
        maybe_module_code: None,
        conf: WorkerRuntimeOpts::EventsWorker(EventWorkerRuntimeOpts {}),
    })
    .await?;

    Ok(events_tx)
}

pub async fn create_user_worker_pool(
    worker_event_sender: Option<mpsc::UnboundedSender<WorkerEventWithMetadata>>,
) -> Result<mpsc::UnboundedSender<UserWorkerMsgs>, Error> {
    let (user_worker_msgs_tx, mut user_worker_msgs_rx) =
        mpsc::unbounded_channel::<UserWorkerMsgs>();

    let user_worker_msgs_tx_clone = user_worker_msgs_tx.clone();

    let _handle: tokio::task::JoinHandle<Result<(), Error>> = tokio::spawn(async move {
        let mut worker_pool = WorkerPool::new(worker_event_sender, user_worker_msgs_tx_clone);

        loop {
            match user_worker_msgs_rx.recv().await {
                None => break,
                Some(UserWorkerMsgs::Create(worker_options, tx)) => {
                    let _ = worker_pool.create_worker(worker_options, tx).await;
                }
                Some(UserWorkerMsgs::SendRequest(key, req, tx)) => {
                    worker_pool.send_request(key, req, tx);
                }
                Some(UserWorkerMsgs::Shutdown(key)) => {
                    worker_pool.shutdown(key);
                }
            }
        }

        Ok(())
    });

    Ok(user_worker_msgs_tx)
}
